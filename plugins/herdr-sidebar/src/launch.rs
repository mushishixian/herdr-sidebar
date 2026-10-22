//! Pure decision/layout helpers used by the native launcher and exposed through
//! stdin→stdout diagnostic modes. Keeping the parsing here makes the behavior
//! unit-testable and validates ids/paths before they reach an API request.
//!
//! - `--launch-decision`: `herdr pane list` JSON → `OPEN` | `FOCUS <pane_id>` |
//!   `CLOSE <pane_id>`, scoped to the focused pane's tab (toggle behavior).
//! - `--focused-pane`: `herdr pane list` JSON → `<pane_id>\t<cwd>` of the focused
//!   pane (cwd stripped of the `\\?\` verbatim prefix herdr reports on Windows).
//! - `--open-plan`: `herdr pane layout` JSON → `<edge_pane_id>\t<ratio>\t<swap>`,
//!   the split target, original-pane share, and whether the configured dock
//!   side needs a swap.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

/// The pane label the launcher assigns (`pane rename`) and later looks for.
pub const PANE_LABEL: &str = "Explorer";

/// Source id for `pane.report_metadata`; its token marks a pane as the
/// Explorer independently of the (cosmetic, clearable) label.
pub const METADATA_SOURCE: &str = "herdr-sidebar-explorer";

/// Present only while a TUI is starting. Windows launchers stamp it after a
/// raw split; directly spawned Unix TUIs stamp it at process startup. The
/// first full identity report clears it, so a pane carrying this token cannot
/// hold unsaved in-memory state yet.
pub const STARTING_TOKEN: &str = "herdr-sidebar-starting";

/// Metadata token the IDE layer (`dotfiles/`) stamps on its Terminal pane
/// before `pane move --new-tab` parks it in a background tab. Its value is the
/// IDE tab id the pane belongs to. While it is set, the pane's tab is a
/// parking tab: quiet hooks must not dock a sidebar there (the sidebar would
/// keep the tab alive after the Terminal moves back), and the parked pane stays
/// an eligible cwd sibling of its home tab. The IDE clears it (null) on return.
pub const PARKED_TOKEN: &str = "ide-parked-for";

const MIN_SIDEBAR_SHARE: f64 = 0.15;
const MAX_SIDEBAR_SHARE: f64 = 0.5;

/// How close (in terminal columns) a dragged pane's share may sit to one of
/// the 15%/50% share bounds before the width reads as geometry-limited rather
/// than chosen.
pub const DRAG_SHARE_EPSILON_COLS: f64 = 1.0;

#[derive(Deserialize)]
struct PaneListMsg {
    result: PaneListResult,
}

#[derive(Deserialize)]
struct PaneListResult {
    #[serde(default)]
    panes: Vec<Pane>,
}

#[derive(Deserialize)]
struct Pane {
    pane_id: Option<String>,
    label: Option<String>,
    cwd: Option<String>,
    /// Live cwd of the foreground process. Unlike `cwd`, this changes after
    /// a shell `cd` or when an agent switches projects.
    foreground_cwd: Option<String>,
    #[serde(default)]
    focused: bool,
    tab_id: Option<String>,
    workspace_id: Option<String>,
    /// Metadata tokens reported via `pane.report_metadata`; shape of the
    /// values is host-defined, only key presence matters here.
    #[serde(default)]
    tokens: serde_json::Map<String, serde_json::Value>,
}

/// A non-sidebar pane in the same tab whose live cwd can be followed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SiblingCwd {
    pane_id: String,
    cwd: String,
    focused: bool,
}

/// Process-local state shared by the Explorer and Source Control views.
///
/// The follower re-roots only when an already-observed sibling's live
/// `foreground_cwd` CHANGES. Focus moves, pane-list reordering and new panes
/// never re-root. The first sample after construction (or `reset`) keeps a
/// sibling that already sits at the current root, and otherwise adopts the
/// focused sibling (lowest pane id as the fallback). Among simultaneous
/// changes the focused one wins, then the previously followed one, then the
/// lowest pane id.
#[derive(Default, Debug)]
pub struct CwdFollower {
    seen: BTreeMap<String, String>,
    selected: Option<String>,
    initialized: bool,
    manual_override: bool,
}

impl CwdFollower {
    /// Record a successful user-selected folder. It only matters for the next
    /// first sample, which then adopts nothing; it is released by the next
    /// observed sibling change.
    pub fn mark_manual_folder(&mut self) {
        self.manual_override = true;
    }

    /// Start following afresh after the persisted setting is enabled.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Return the next live cwd to follow, if this sample changes the chosen
    /// root according to the rules on [`CwdFollower`].
    pub fn next_cwd(
        &mut self,
        pane_list_json: &str,
        my_pane_id: &str,
        current_root: &Path,
    ) -> Option<String> {
        let siblings = sibling_cwds(pane_list_json, my_pane_id);
        let changed = siblings
            .iter()
            .filter(|s| self.seen.get(&s.pane_id).is_some_and(|old| old != &s.cwd))
            .collect::<Vec<_>>();
        let first_sample = !self.initialized;
        let manual = std::mem::take(&mut self.manual_override);
        self.seen = siblings
            .iter()
            .map(|s| (s.pane_id.clone(), s.cwd.clone()))
            .collect();
        self.initialized = true;

        if first_sample {
            if manual {
                return None;
            }
            if let Some(at_root) = siblings.iter().find(|s| Path::new(&s.cwd) == current_root) {
                self.selected = Some(at_root.pane_id.clone());
                return None;
            }
            let all = siblings.iter().collect::<Vec<_>>();
            let picked = pick_sibling(&all, None)?;
            self.selected = Some(picked.pane_id.clone());
            return Some(picked.cwd.clone());
        }

        let picked = pick_sibling(&changed, self.selected.as_deref())?;
        self.selected = Some(picked.pane_id.clone());
        self.manual_override = false;
        Some(picked.cwd.clone())
    }
}

fn pick_sibling<'a>(siblings: &[&'a SiblingCwd], selected: Option<&str>) -> Option<&'a SiblingCwd> {
    siblings
        .iter()
        .copied()
        .find(|s| s.focused)
        .or_else(|| {
            siblings
                .iter()
                .copied()
                .find(|s| Some(s.pane_id.as_str()) == selected)
        })
        .or_else(|| siblings.first().copied())
}

impl Pane {
    /// An Explorer is recognized by its metadata token (reported by the TUI at
    /// startup — survives the label being cleared while collapsed) or by the
    /// "Explorer" label (retained for restored panes whose metadata does not
    /// survive a Herdr server restart).
    fn is_explorer(&self) -> bool {
        self.tokens.contains_key(METADATA_SOURCE)
            || self.label.as_deref() == Some(PANE_LABEL)
            || self.label.as_deref() == Some(SIDEBAR_LABEL)
    }

    /// One of OUR labels with NO heartbeat token is a corpse. The main way
    /// this happens: herdr resumes a restarted server's panes with their
    /// labels and scrollback, but the process inside is a fresh shell and
    /// metadata tokens do not survive. Native launches hold the shared lock
    /// until identity is stamped, so queued hooks never mistake a fresh pane
    /// for one of these restored corpses.
    fn our_label_without_token(&self) -> bool {
        matches!(self.label.as_deref(), Some("Sidebar" | "Explorer"))
            && !self.tokens.contains_key(METADATA_SOURCE)
    }
}

/// The unified pane's label (mirrors state::SIDEBAR_LABEL; kept here so the
/// launch module stays dependency-free for its tests).
const SIDEBAR_LABEL: &str = "Sidebar";

#[derive(Deserialize)]
struct LayoutMsg {
    result: LayoutResult,
}

#[derive(Deserialize)]
struct LayoutResult {
    layout: Layout,
}

#[derive(Deserialize)]
struct Layout {
    #[serde(default)]
    panes: Vec<LayoutPane>,
    #[serde(default)]
    splits: Vec<LayoutSplit>,
    area: Option<Rect>,
    tab_id: Option<String>,
    #[serde(default)]
    zoomed: bool,
}

#[derive(Deserialize)]
struct LayoutSplit {
    /// `split_<n>_root` / `split_<n>_010`: herdr encodes the split's tree path
    /// in its id (see [`split_path`]). Absent in hosts older than 0.9, which
    /// simply makes that split unaddressable.
    id: Option<String>,
    direction: Option<String>,
    ratio: Option<f64>,
    rect: Option<Rect>,
}

#[derive(Deserialize)]
struct LayoutPane {
    pane_id: Option<String>,
    rect: Option<Rect>,
}

#[derive(Deserialize)]
struct Rect {
    x: i64,
    y: i64,
    width: i64,
    #[serde(default)]
    height: i64,
}

/// Windows PowerShell 5.1 prepends a UTF-8 BOM when piping into a native
/// process's stdin (verified live); serde_json rejects a BOM before `{`.
pub(crate) fn strip_bom(input: &str) -> &str {
    input.trim_start_matches('\u{feff}')
}

/// A live TUI re-stamps its identity token with the unix time every few
/// seconds; a token older than this is a DEAD pane (its process was killed
/// out from under it) and should be replaced, not focused. process_info
/// can't tell the difference on Windows — the TUI child never shows in the
/// pane's foreground group (verified live).
pub const HEARTBEAT_STALE_SECS: u64 = 20;

/// True when `key` is present but its heartbeat timestamp is missing,
/// unparsable, or older than [`HEARTBEAT_STALE_SECS`]. Absent key = false
/// (a fresh pane the launcher labeled but whose TUI hasn't reported yet).
fn token_stale(tokens: &serde_json::Map<String, serde_json::Value>, key: &str, now: u64) -> bool {
    let Some(value) = tokens.get(key) else {
        return false;
    };
    let ts = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()));
    match ts {
        Some(ts) => now.saturating_sub(ts) > HEARTBEAT_STALE_SECS,
        None => true, // pre-heartbeat token format: treat as dead once seen
    }
}

/// `OPEN`, `FOCUS <id>`, `CLOSE <id>`, or `REPLACE <id>` (dead pane: close
/// it, then open fresh) from a `pane list` JSON. Unparseable input, no
/// focused pane, or an unsafe id all degrade to `OPEN` — the safe default is
/// a fresh explorer, never acting on a pane in an unknown tab.
pub fn launch_decision(pane_list_json: &str, now: u64) -> String {
    launch_decision_in(pane_list_json, now, "")
}

/// [`launch_decision`] confined to `scope` (a tab or workspace id), so the
/// decision reasons about the tab the hook is actually docking into. An
/// empty scope keeps the global behavior.
///
/// This MUST use the same scope as [`focused_pane_in`]: deciding against the
/// focused tab while docking into another one answers OPEN for a tab that
/// already has a sidebar, and twins it.
pub fn launch_decision_in(pane_list_json: &str, now: u64, scope: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return "OPEN".to_string();
    };
    let panes = &msg.result.panes;
    let in_scope = |p: &&Pane| {
        scope.is_empty()
            || p.tab_id.as_deref() == Some(scope)
            || p.workspace_id.as_deref() == Some(scope)
    };
    let Some(focused) = panes
        .iter()
        .find(|p| in_scope(p) && p.focused)
        .or_else(|| panes.iter().find(in_scope))
    else {
        return "OPEN".to_string();
    };
    let explorer = panes
        .iter()
        .find(|p| p.is_explorer() && p.tab_id.as_deref() == focused.tab_id.as_deref());
    let Some(pane) = explorer else {
        return "OPEN".to_string();
    };
    let Some(id) = pane.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return "OPEN".to_string();
    };
    if token_stale(&pane.tokens, METADATA_SOURCE, now) || pane.our_label_without_token() {
        return format!("REPLACE {id}");
    }
    if Some(id) == focused.pane_id.as_deref() {
        format!("CLOSE {id}")
    } else {
        format!("FOCUS {id}")
    }
}

/// Whether the tab that `scope` resolves to is a parking tab: it has at least
/// one pane and EVERY pane in it carries a non-empty [`PARKED_TOKEN`]. The tab
/// is resolved exactly as [`launch_decision_in`] does (focused pane in scope,
/// else first pane in scope; an empty scope is the globally focused pane).
pub fn in_parking_tab(pane_list_json: &str, scope: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return false;
    };
    let panes = &msg.result.panes;
    let in_scope = |p: &&Pane| {
        scope.is_empty()
            || p.tab_id.as_deref() == Some(scope)
            || p.workspace_id.as_deref() == Some(scope)
    };
    let Some(focused) = panes
        .iter()
        .find(|p| in_scope(p) && p.focused)
        .or_else(|| panes.iter().find(in_scope))
    else {
        return false;
    };
    let Some(tab) = focused.tab_id.as_deref() else {
        return false;
    };
    let mut in_tab = panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(tab))
        .peekable();
    in_tab.peek().is_some()
        && in_tab.all(|p| {
            p.tokens
                .get(PARKED_TOKEN)
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.is_empty())
        })
}

/// Strict-toggle mapping (⚙ Settings): a `FOCUS` decision becomes `CLOSE`,
/// so one toggle press opens and the next press closes, wherever focus is.
/// Every other decision passes through untouched; kept pure so both the CLI
/// mode and the Windows ensure sidecar share one tested mapping.
pub fn focus_as_close(decision: &str) -> String {
    match decision.split_once(' ') {
        Some(("FOCUS", id)) => format!("CLOSE {id}"),
        _ => decision.to_string(),
    }
}

/// Source-control identity for the separated Source Control pane.
pub const SC_PANE_LABEL: &str = "Source Control";
pub const SC_METADATA_SOURCE: &str = "herdr-sidebar-git";
const PREVIEW_METADATA_SOURCE: &str = "herdr-sidebar-preview";

/// Like [`launch_decision`], but for the separated Source Control pane (the
/// unified Sidebar pane carries BOTH tokens, so it satisfies this too).
pub fn launch_decision_git(pane_list_json: &str, now: u64) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return "OPEN".to_string();
    };
    let panes = &msg.result.panes;
    let Some(focused) = panes.iter().find(|p| p.focused) else {
        return "OPEN".to_string();
    };
    let panel = panes.iter().find(|p| {
        (p.tokens.contains_key(SC_METADATA_SOURCE) || p.label.as_deref() == Some(SC_PANE_LABEL))
            && p.tab_id.as_deref() == focused.tab_id.as_deref()
    });
    let Some(pane) = panel else {
        return "OPEN".to_string();
    };
    let Some(id) = pane.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return "OPEN".to_string();
    };
    if token_stale(&pane.tokens, SC_METADATA_SOURCE, now)
        || (pane.label.as_deref() == Some(SC_PANE_LABEL)
            && !pane.tokens.contains_key(SC_METADATA_SOURCE))
    {
        return format!("REPLACE {id}");
    }
    if Some(id) == focused.pane_id.as_deref() {
        format!("CLOSE {id}")
    } else {
        format!("FOCUS {id}")
    }
}

/// Whether `pane_id` carries any of our identity tokens.
pub fn pane_has_token(pane_list_json: &str, pane_id: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return false;
    };
    msg.result
        .panes
        .iter()
        .filter(|p| p.pane_id.as_deref() == Some(pane_id))
        .any(|p| {
            p.tokens.contains_key(METADATA_SOURCE) || p.tokens.contains_key(SC_METADATA_SOURCE)
        })
}

/// Whether `pane_id` has started but not yet reached the TUI event loop. The
/// first full identity report clears this marker.
pub fn pane_is_starting(pane_list_json: &str, pane_id: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return false;
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))
        .is_some_and(|p| p.tokens.contains_key(STARTING_TOKEN))
}

/// `<pane_id>\t<cwd>` of the focused pane, or empty on any failure. The cwd
/// keeps its spaces (hence the tab separator) but loses any `\\?\` verbatim
/// prefix.
pub fn focused_pane(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|pane| pane.focused)
        .map(pane_spawn_target)
        .unwrap_or_default()
}

fn pane_spawn_target(pane: &Pane) -> String {
    let Some(id) = pane.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return String::new();
    };
    let cwd = pane
        .foreground_cwd
        .as_deref()
        .or(pane.cwd.as_deref())
        .map(strip_verbatim)
        .unwrap_or_default();
    format!("{id}\t{cwd}")
}

pub(crate) fn is_preview_label(label: &str) -> bool {
    label == "Preview"
        || label.starts_with("Preview · ")
        || label.starts_with("Editor · ")
        || label.ends_with(" · preview")
        || label.ends_with(" · editor")
}

/// Eligible panes in our tab, plus any pane parked for our tab
/// ([`PARKED_TOKEN`] naming it, wherever it currently lives), sorted by pane
/// id so the fallback is stable. `foreground_cwd` is deliberately required:
/// falling back to `cwd` would resurrect a stale spawn directory when the host
/// has no live cwd signal.
fn sibling_cwds(pane_list_json: &str, my_pane_id: &str) -> Vec<SiblingCwd> {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return Vec::new();
    };
    let panes = &msg.result.panes;
    let Some(me) = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(my_pane_id))
    else {
        return Vec::new();
    };
    let my_tab = me.tab_id.as_deref();
    let mut siblings = panes
        .iter()
        .filter(|p| {
            let in_my_tab = p.tab_id.as_deref() == my_tab;
            let parked_for_me = my_tab.is_some()
                && p.tokens.get(PARKED_TOKEN).and_then(|v| v.as_str()) == my_tab;
            (in_my_tab || parked_for_me)
                && p.pane_id.as_deref() != Some(my_pane_id)
                && !p.is_explorer()
                && !p.tokens.contains_key(SC_METADATA_SOURCE)
                && !p.tokens.contains_key(PREVIEW_METADATA_SOURCE)
                && p.label.as_deref() != Some(SC_PANE_LABEL)
                && !p.label.as_deref().is_some_and(is_preview_label)
        })
        .filter_map(|p| {
            let pane_id = p.pane_id.as_deref().filter(|id| is_flag_safe(id))?;
            let cwd = p.foreground_cwd.as_deref().map(strip_verbatim)?;
            (!cwd.is_empty()).then(|| SiblingCwd {
                pane_id: pane_id.to_string(),
                cwd: cwd.to_string(),
                focused: p.focused,
            })
        })
        .collect::<Vec<_>>();
    siblings.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    siblings
}

/// `<pane_id>\t<ratio>\t<swap>` for the configured edge. Left docking splits
/// the leftmost pane and swaps; right docking splits the rightmost pane with
/// an inverted original-pane ratio and needs no swap. Empty on any failure.
pub fn open_plan(layout_json: &str, dock_right: bool, target_cols: u16) -> String {
    let Ok(msg) = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)) else {
        return String::new();
    };
    let mut best: Option<(&str, &Rect)> = None;
    for pane in &msg.result.layout.panes {
        let (Some(id), Some(rect)) = (pane.pane_id.as_deref(), pane.rect.as_ref()) else {
            continue;
        };
        if !is_flag_safe(id) || rect.width <= 0 {
            continue;
        }
        // The configured edge wins; among a stacked edge column, topmost wins.
        let better = match best {
            None => true,
            Some((_, b)) if dock_right => (rect.x + rect.width, -rect.y) > (b.x + b.width, -b.y),
            Some((_, b)) => (rect.x, rect.y) < (b.x, b.y),
        };
        if better {
            best = Some((id, rect));
        }
    }
    let Some((id, rect)) = best else {
        return String::new();
    };
    let sidebar_share =
        (f64::from(target_cols) / rect.width as f64).clamp(MIN_SIDEBAR_SHARE, MAX_SIDEBAR_SHARE);
    let ratio = if dock_right {
        1.0 - sidebar_share
    } else {
        sidebar_share
    };
    format!("{id}\t{ratio:.6}\t{}", !dock_right)
}

/// Width of the whole tab area that owns a pane split. A pane-only divider
/// resize leaves this unchanged; terminal chrome/sidebar changes do not.
pub fn layout_width(layout_json: &str) -> Option<i64> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    msg.result.layout.area.map(|area| area.width)
}

/// The pane's new width, in terminal columns, when the only difference between
/// two `pane layout` snapshots is this pane's own divider — i.e. the user
/// dragged it. `None` for anything else (window resize, split/close, zoom, an
/// unparsable snapshot), and `None` when the resulting share sits on the
/// MIN/MAX_SIDEBAR_SHARE clamp, where the width is geometry-limited rather than
/// chosen (adopting it would corrupt the stored target on a narrow tab).
///
/// The caller still clamps to the settings range; this helper only decides
/// whether the drag was an intentional width choice at all.
pub fn dragged_width(before: &str, after: &str, pane_id: &str, term_cols_now: u16) -> Option<u16> {
    if pane_id.is_empty() {
        return None;
    }
    let before = serde_json::from_str::<LayoutMsg>(strip_bom(before))
        .ok()?
        .result
        .layout;
    let after = serde_json::from_str::<LayoutMsg>(strip_bom(after))
        .ok()?
        .result
        .layout;
    if before.zoomed || after.zoomed {
        return None;
    }
    let before_area = before.area.as_ref()?;
    let after_area = after.area.as_ref()?;
    if before_area.width != after_area.width {
        return None;
    }
    // A split, a close, or a preview pane appearing changes the pane set.
    let pane_set = |layout: &Layout| -> Vec<String> {
        let mut ids: Vec<String> = layout
            .panes
            .iter()
            .filter_map(|pane| pane.pane_id.clone())
            .collect();
        ids.sort();
        ids
    };
    if pane_set(&before) != pane_set(&after) {
        return None;
    }
    let before_rect = before
        .panes
        .iter()
        .find(|pane| pane.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    let after_rect = after
        .panes
        .iter()
        .find(|pane| pane.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    if before_rect.width == after_rect.width {
        return None;
    }
    // Either dock side: the divider is the pane's left or right edge. Take the
    // first edge carrying a covering horizontal split (the dock side is not
    // needed for the clamp check).
    let split = [after_rect.x + after_rect.width, after_rect.x]
        .into_iter()
        .find_map(|divider_x| {
            after
                .splits
                .iter()
                .filter(|split| split.direction.as_deref() == Some("right"))
                .filter_map(|split| Some((split.rect.as_ref()?, split.ratio?)))
                .filter(|(rect, ratio)| {
                    let split_divider =
                        rect.x + (f64::from(rect.width as i32) * ratio).round() as i64;
                    rect.x <= after_rect.x
                        && (split_divider - divider_x).abs() <= 2
                        && rect.width > 0
                })
                .min_by_key(|(rect, _)| rect.width)
        })?;
    let share = after_rect.width as f64 / split.0.width as f64;
    let epsilon = DRAG_SHARE_EPSILON_COLS / split.0.width as f64;
    if (share - MIN_SIDEBAR_SHARE).abs() <= epsilon || (share - MAX_SIDEBAR_SHARE).abs() <= epsilon
    {
        return None;
    }
    Some(term_cols_now)
}

/// The tab (preferred) or workspace the event concerns, from
/// `HERDR_PLUGIN_EVENT_JSON`; "" when the payload names neither.
///
/// The ensure hook used to root a new sidebar in the GLOBALLY focused pane's
/// cwd, which during a workspace switch is still the space you came from —
/// observed live as tremor's sidebar rooted in bedrock. The payload knows
/// which tab is being docked; this is that answer.
pub fn event_scope(event_json: &str) -> String {
    event_scope_in(event_json, "")
}

/// [`event_scope`] with a pane snapshot available to resolve `pane.focused`.
/// That event carries a pane id but no tab id, and treating its workspace id
/// as a scope lets the globally focused pane from any sibling tab win.
pub fn event_scope_in(event_json: &str, pane_list_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(strip_bom(event_json)) else {
        return String::new();
    };
    let data = value.get("data").unwrap_or(&value);
    let pick = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(|v| v.as_str())
            .or_else(|| {
                // workspace_created carries a nested WorkspaceInfo.
                data.get(key.trim_end_matches("_id"))
                    .and_then(|w| w.get(key))
                    .and_then(|v| v.as_str())
            })
            .map(str::to_string)
    };
    let pane_id = pick("pane_id");
    let tab_from_pane = pane_id.as_deref().and_then(|pane_id| {
        let msg = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)).ok()?;
        msg.result
            .panes
            .iter()
            .find(|pane| pane.pane_id.as_deref() == Some(pane_id))?
            .tab_id
            .clone()
    });
    pick("tab_id")
        .or(tab_from_pane)
        .or_else(|| pick("workspace_id"))
        .filter(|s| is_flag_safe(s))
        .unwrap_or_default()
}

pub fn event_scope_with_tab_context(
    event_json: &str,
    pane_list_json: &str,
    context_tab: &str,
) -> String {
    let scope = event_scope_in(event_json, pane_list_json);
    let context_matches = is_flag_safe(context_tab)
        && context_tab.contains(':')
        && (scope.is_empty()
            || scope == context_tab
            || (!scope.contains(':') && context_tab.starts_with(&format!("{scope}:"))));
    if context_matches {
        context_tab.to_string()
    } else {
        scope
    }
}

/// The pane whose cwd a sidebar docked into `scope` should be rooted from:
/// the focused pane WITHIN that scope, else any pane in it (a brand-new space
/// may not have a focused pane yet). An empty scope keeps the old global
/// behavior. Returns `<pane_id>\t<cwd>`, or "" when the scope has no panes.
pub fn focused_pane_in(pane_list_json: &str, scope: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    let panes = &msg.result.panes;
    let workspace_tab = (!scope.is_empty() && !scope.contains(':'))
        .then(|| {
            panes
                .iter()
                .filter(|pane| pane.workspace_id.as_deref() == Some(scope))
                .filter_map(|pane| pane.tab_id.as_deref())
                .collect::<BTreeSet<_>>()
        })
        .filter(|tabs| tabs.len() == 1)
        .and_then(|tabs| tabs.into_iter().next());
    let in_scope = |p: &&Pane| {
        scope.is_empty()
            || p.tab_id.as_deref() == Some(scope)
            || workspace_tab.is_some_and(|tab_id| p.tab_id.as_deref() == Some(tab_id))
    };
    let Some(chosen) = panes
        .iter()
        .find(|p| in_scope(p) && p.focused)
        .or_else(|| panes.iter().find(in_scope))
    else {
        return String::new();
    };
    pane_spawn_target(chosen)
}

/// Which event invoked the ensure hook, from `HERDR_PLUGIN_EVENT_JSON`.
///
/// All launcher hooks run the same native implementation, so the payload is the only way to
/// treat space creation differently from an ordinary focus. The envelope
/// `EventEnvelope` currently serializes the discriminator as lower_snake in
/// `event`, while manifest hook names use dotted form. Both are accepted, and
/// the usual nested wrappers remain supported for older payload shapes.
///
/// The result is interpolated into a shell command, so it is restricted to a
/// plain `lower_snake` identifier; anything else yields "".
pub fn event_kind(event_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(strip_bom(event_json)) else {
        return String::new();
    };
    let kind = ["type", "data", "event"]
        .iter()
        .find_map(|key| match value.get(key) {
            Some(serde_json::Value::String(s)) => Some(s.as_str()),
            Some(inner) => inner.get("type").and_then(|v| v.as_str()),
            None => None,
        })
        .unwrap_or_default();
    let kind = kind.replace('.', "_");
    let safe = !kind.is_empty()
        && kind.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        && kind.len() <= 64;
    if safe { kind } else { String::new() }
}

/// A workspace's label from a `workspace list` JSON, empty when unknown.
///
/// The label is how a remembered tree root is keyed: workspace IDs identify a
/// space *instance* and get reassigned when a space is closed and recreated,
/// so they cannot carry a choice across that boundary.
pub fn workspace_label(workspace_list_json: &str, workspace_id: &str) -> String {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        #[serde(default)]
        workspaces: Vec<Ws>,
    }
    #[derive(Deserialize)]
    struct Ws {
        workspace_id: Option<String>,
        label: Option<String>,
    }
    if workspace_id.is_empty() {
        return String::new();
    }
    serde_json::from_str::<Msg>(strip_bom(workspace_list_json))
        .ok()
        .and_then(|msg| {
            msg.result
                .workspaces
                .into_iter()
                .find(|w| w.workspace_id.as_deref() == Some(workspace_id))
                .and_then(|w| w.label)
        })
        .unwrap_or_default()
}

/// Label the plugin gives a workspace's generated first tab.
pub const FIRST_TAB_LABEL: &str = "main";

/// The tab fields the first-tab rename cares about, shared by `tab.get` and
/// `tab.list` replies.
#[derive(Deserialize)]
struct Tab {
    tab_id: Option<String>,
    workspace_id: Option<String>,
    label: Option<String>,
    number: Option<u64>,
}

/// The tab id of herdr's *untouched* first tab, else `None`.
///
/// herdr labels a workspace's initial tab with its position ("1") and the tab
/// bar renders that label. Both halves matter: a number above 1 belongs to a
/// later tab, and a label that differs from the number was chosen by someone
/// (a human, or this plugin's preview suffix), so it is left alone.
fn generated_first_tab(tab: &Tab) -> Option<String> {
    let tab_id = tab.tab_id.as_deref().filter(|id| !id.is_empty())?;
    let number = tab.number?;
    let label = tab.label.as_deref()?;
    (number == 1 && label == number.to_string()).then(|| tab_id.to_string())
}

/// `(tab_id, label)` to rename for herdr's generated first tab, else `None`.
pub fn first_tab_rename(tab_info_json: &str) -> Option<(String, &'static str)> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        tab: Option<Tab>,
    }
    let msg = serde_json::from_str::<Msg>(strip_bom(tab_info_json)).ok()?;
    Some((generated_first_tab(&msg.result.tab?)?, FIRST_TAB_LABEL))
}

/// The tab id of `workspace_id`'s generated first tab from a `tab.list` reply
/// (`{"result":{"tabs":[..]}}`), else `None`.
///
/// `workspace_created` payloads name a workspace but not a tab, so the target
/// has to be found in the workspace's tab list instead of guessed from the
/// globally focused tab.
pub fn generated_first_tab_in(tab_list_json: &str, workspace_id: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        #[serde(default)]
        tabs: Vec<Tab>,
    }
    if workspace_id.is_empty() {
        return None;
    }
    let msg = serde_json::from_str::<Msg>(strip_bom(tab_list_json)).ok()?;
    msg.result
        .tabs
        .iter()
        .filter(|tab| tab.workspace_id.as_deref() == Some(workspace_id))
        .find_map(generated_first_tab)
}

/// The focused pane's tab id from a `pane list` JSON (flag-safe, else empty).
pub fn focused_tab(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.focused)
        .and_then(|p| p.tab_id.clone())
        .filter(|t| is_flag_safe(t))
        .unwrap_or_default()
}

/// The focused pane's workspace id from a `pane list` JSON (flag-safe, else
/// empty). Creation hooks use this to avoid focusing a workspace or tab the
/// caller deliberately created in the background.
pub fn focused_workspace(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.focused)
        .and_then(|p| {
            p.workspace_id.clone().or_else(|| {
                p.tab_id
                    .as_deref()
                    .and_then(|tab| tab.split_once(':'))
                    .map(|(workspace, _)| workspace.to_string())
            })
        })
        .filter(|workspace| is_flag_safe(workspace))
        .unwrap_or_default()
}

/// The tab containing `pane_id` ("" when absent) — the hide path snoozes it.
pub fn tab_of(pane_list_json: &str, pane_id: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))
        .and_then(|p| p.tab_id.clone())
        .unwrap_or_default()
}

/// Any pane living in `tab_id` ("" when the tab is empty or unknown). The
/// preview flow needs one because herdr 0.9 moves the VIEWING client only on
/// `pane.focus`; `tab.focus` updates the session-wide record and nothing else.
pub fn pane_in_tab(pane_list_json: &str, tab_id: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.tab_id.as_deref() == Some(tab_id))
        .and_then(|p| p.pane_id.clone())
        .unwrap_or_default()
}

/// The pane id the SERVER currently records as focused ("" when none) —
/// unlike `focused_pane`, no cwd, no flag-safety filter.
pub fn server_focused_pane_id(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.focused)
        .and_then(|p| p.pane_id.clone())
        .unwrap_or_default()
}

/// Any pane NOT in `tab_id` ("" when there is none) — a stepping stone when a
/// focus transition must be forced.
pub fn pane_outside_tab(pane_list_json: &str, tab_id: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.tab_id.as_deref() != Some(tab_id))
        .and_then(|p| p.pane_id.clone())
        .unwrap_or_default()
}

/// The workspace a pane belongs to, empty when the pane is unknown. Preview
/// routing is scoped by it so one project's ephemeral tab is never reused
/// from another.
pub fn workspace_of(pane_list_json: &str, pane_id: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))
        .and_then(|p| p.workspace_id.clone())
        .unwrap_or_default()
}

/// All tab ids present in a `pane list` JSON — the live-tab set the snooze
/// cleanup checks markers against.
pub fn live_tabs(pane_list_json: &str) -> std::collections::BTreeSet<String> {
    serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json))
        .map(|msg| {
            msg.result
                .panes
                .into_iter()
                .filter_map(|p| p.tab_id)
                .collect()
        })
        .unwrap_or_default()
}

/// The created pane's id from a `pane.split` response
/// (`{"result":{"pane":{"pane_id":..}}}`), validated flag-safe.
pub fn split_pane_id(response_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        pane: Option<Info>,
    }
    #[derive(Deserialize)]
    struct Info {
        pane_id: Option<String>,
    }
    serde_json::from_str::<Msg>(strip_bom(response_json))
        .ok()?
        .result
        .pane?
        .pane_id
        .filter(|id| is_flag_safe(id))
}

/// `(tab_id, root_pane_id)` from a `tab.create` response
/// (`{"result":{"tab":{"tab_id":..},"root_pane":{"pane_id":..}}}`), both
/// validated flag-safe. The root pane is the tab's one shell pane — the
/// preview flow drives it as the viewer instead of splitting a second one.
pub fn created_tab_root_pane(response_json: &str) -> Option<(String, String)> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        tab: Option<Tab>,
        root_pane: Option<Pane>,
    }
    #[derive(Deserialize)]
    struct Tab {
        tab_id: Option<String>,
    }
    #[derive(Deserialize)]
    struct Pane {
        pane_id: Option<String>,
    }
    let res = serde_json::from_str::<Msg>(strip_bom(response_json))
        .ok()?
        .result;
    let tab_id = res.tab?.tab_id.filter(|id| is_flag_safe(id))?;
    let pane_id = res.root_pane?.pane_id.filter(|id| is_flag_safe(id))?;
    Some((tab_id, pane_id))
}

/// The created pane id from a `plugin.pane.open` response. Direct plugin-pane
/// spawning starts the manifest argv without showing an intermediary shell.
pub fn plugin_pane_id(response_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        plugin_pane: PluginPane,
    }
    #[derive(Deserialize)]
    struct PluginPane {
        pane: Info,
    }
    #[derive(Deserialize)]
    struct Info {
        pane_id: Option<String>,
    }
    serde_json::from_str::<Msg>(strip_bom(response_json))
        .ok()?
        .result
        .plugin_pane
        .pane
        .pane_id
        .filter(|id| is_flag_safe(id))
}

/// One step of the full-height repair: `below` (a pane under the sidebar,
/// truncating its column) should be re-parented as a down-split of `beside`
/// (the pane toward the tab interior) in `tab`.
pub struct RepairStep {
    pub below: String,
    pub beside: String,
    pub tab: String,
}

/// When `pane_id` is not a full-height column of its tab, find the pane
/// directly below it and the pane beside it toward the tab interior — moving
/// the former under the latter (via a bounce through a temp tab; herdr no-ops
/// same-tab moves) grows the sidebar to full height. `None` when already full
/// height or the layout doesn't match. Called in a loop: each step removes one
/// pane from under the sidebar.
pub fn repair_step(layout_json: &str, pane_id: &str, dock_right: bool) -> Option<RepairStep> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    let layout = &msg.result.layout;
    let area = layout.area.as_ref()?;
    let tab = layout.tab_id.as_deref().filter(|t| is_flag_safe(t))?;
    let me = layout
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    if me.height >= area.height {
        return None;
    }
    let find = |pred: &dyn Fn(&Rect) -> bool| -> Option<String> {
        layout
            .panes
            .iter()
            .filter(|p| p.pane_id.as_deref() != Some(pane_id))
            .filter_map(|p| Some((p.pane_id.as_deref()?, p.rect.as_ref()?)))
            .find(|(id, rect)| is_flag_safe(id) && pred(rect))
            .map(|(id, _)| id.to_string())
    };
    let below = find(&|r: &Rect| r.y == me.y + me.height && r.x <= me.x && me.x < r.x + r.width)?;
    let beside = if dock_right {
        find(&|r: &Rect| r.x + r.width == me.x && r.y == me.y)?
    } else {
        find(&|r: &Rect| r.x == me.x + me.width && r.y == me.y)?
    };
    Some(RepairStep {
        below,
        beside,
        tab: tab.to_string(),
    })
}

/// One `herdr pane resize` invocation: which way to move our interior edge and by
/// how much. `amount` is a RATIO delta — herdr adds it to the nearest split's
/// ratio (`layout.rs::resize_focused`: `current_ratio ± delta`), it is NOT
/// columns.
pub struct ResizeStep {
    pub direction: &'static str,
    pub amount: f64,
}

/// Compute the resize step that brings `pane_id` from `term_cols_now` to
/// `term_cols_target` terminal columns, from a `pane layout` JSON.
///
/// The sidebar's interior edge is the divider of some horizontal split: right
/// edge for a left dock, left edge for a right dock. Pick the innermost matching
/// split and convert the column delta into that split's ratio space. `None`
/// when the pane/split can't be found or the pane is already at the target.
pub fn resize_plan(
    layout_json: &str,
    pane_id: &str,
    term_cols_now: u16,
    term_cols_target: u16,
    dock_right: bool,
) -> Option<ResizeStep> {
    resize_plan_inner(
        layout_json,
        pane_id,
        term_cols_now,
        term_cols_target,
        dock_right,
        false,
    )
}

/// Re-assert a preferred column width after the tab's available width changes.
/// The target is exact in the normal range, but yields to the same 15%-50%
/// share bounds used when the sidebar first spawns.
pub fn preferred_resize_plan(
    layout_json: &str,
    pane_id: &str,
    term_cols_now: u16,
    preferred_cols: u16,
    dock_right: bool,
) -> Option<ResizeStep> {
    resize_plan_inner(
        layout_json,
        pane_id,
        term_cols_now,
        preferred_cols,
        dock_right,
        true,
    )
}

fn resize_plan_inner(
    layout_json: &str,
    pane_id: &str,
    term_cols_now: u16,
    term_cols_target: u16,
    dock_right: bool,
    clamp_share: bool,
) -> Option<ResizeStep> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    let layout = &msg.result.layout;
    let pane_rect = layout
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    let divider_x = if dock_right {
        pane_rect.x
    } else {
        pane_rect.x + pane_rect.width
    };
    let split = layout
        .splits
        .iter()
        .filter(|s| s.direction.as_deref() == Some("right"))
        .filter_map(|s| Some((s.rect.as_ref()?, s.ratio?)))
        .filter(|(rect, ratio)| {
            let split_divider = rect.x + (f64::from(rect.width as i32) * ratio).round() as i64;
            rect.x <= pane_rect.x && (split_divider - divider_x).abs() <= 2 && rect.width > 0
        })
        .min_by_key(|(rect, _)| rect.width)?;

    // The pane rect can be a couple of columns wider than the terminal inside
    // it (pane chrome); express the target in rect space. Preferred-width
    // reassertion keeps the content usable at extreme tab widths.
    let chrome = pane_rect.width - i64::from(term_cols_now);
    let requested = i64::from(term_cols_target) + chrome.max(0);
    let target_rect_w = if clamp_share {
        let min = (split.0.width as f64 * MIN_SIDEBAR_SHARE).ceil() as i64;
        let max = (split.0.width as f64 * MAX_SIDEBAR_SHARE).floor() as i64;
        requested.clamp(min, max.max(min))
    } else {
        requested
    };

    let delta = (target_rect_w - pane_rect.width) as f64 / split.0.width as f64;
    if delta.abs() < 0.005 {
        return None;
    }
    Some(ResizeStep {
        direction: match (dock_right, delta > 0.0) {
            (false, true) | (true, false) => "right",
            (false, false) | (true, true) => "left",
        },
        amount: delta.abs(),
    })
}

/// Decode a `pane.layout` split id into its tree path. herdr names splits
/// `split_<n>_root` (the tab's root split) or `split_<n>_<bits>`, where every
/// bit is one child step: `1` = the SECOND child, `0` = the first. The path is
/// what `layout.set_split_ratio` takes.
fn split_path(id: &str) -> Option<Vec<bool>> {
    let suffix = id.strip_prefix("split_")?.split_once('_')?.1;
    if suffix == "root" {
        return Some(Vec::new());
    }
    if suffix.is_empty() || !suffix.chars().all(|bit| matches!(bit, '0' | '1')) {
        return None;
    }
    Some(suffix.chars().map(|bit| bit == '1').collect())
}

/// The tab's `down` split that does NOT contain `pane_id` — the IDE layout's
/// editor/terminal column — as (path bools, current ratio, split height rows).
///
/// The ratio is the FIRST (top) child's share, so an editor/terminal column is
/// sized by `1 - rows / height` (see [`ratio_for_rows`]). `None` when the tab
/// has no addressable down split (nothing to size) or the layout is
/// unreadable. A split whose rect CONTAINS our pane is skipped: sizing our own
/// column is what `sidebar_width` does. When several qualify the smallest rect
/// wins (innermost), tie-broken by path, so the choice is deterministic.
pub fn foreign_down_split(layout_json: &str, pane_id: &str) -> Option<(Vec<bool>, f64, i64)> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    let layout = &msg.result.layout;
    let mine = pane_rect(layout, pane_id)?;
    layout
        .splits
        .iter()
        .filter(|s| s.direction.as_deref() == Some("down"))
        .filter_map(|s| {
            let rect = s.rect.as_ref()?;
            let ratio = s.ratio?;
            let path = split_path(s.id.as_deref()?)?;
            (!rect_holds(rect, mine) && rect.height > 0)
                .then(|| (rect.width * rect.height, path, ratio, rect.height))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, path, ratio, height)| (path, ratio, height))
}

/// The tab's `right` split that sizes the RIGHTMOST column — the IDE layout's
/// Agent column — as (path bools, current ratio, split width cols).
///
/// The ratio is the FIRST (left/middle) child's share, so an Agent column of N
/// columns inside a `width`-wide split is sized by `1 - cols / width` (see
/// [`ratio_for_cols`]). `None` when the tab has no addressable right split
/// (nothing to size) or the layout is unreadable.
///
/// Candidates are the right splits whose RIGHT child is not our own column
/// (that one is `sidebar_width`'s job, and skipping it is what lets a
/// RIGHT-docked sidebar still resolve the Agent boundary), and whose right child
/// nests no other right split — i.e. the boundary of the rightmost column, not
/// one inside it. The sidebar is normally NESTED in the same left subtree as
/// the middle column, so the Agent boundary is often the tab's root split and
/// does contain our pane: containment alone must not reject it (that bug made
/// the row refuse to size the common `[[sidebar|editor] | agent]` shape).
/// The largest remaining rect wins, tie-broken by path, so the choice never
/// depends on the order herdr listed the splits in.
pub fn foreign_right_split(layout_json: &str, pane_id: &str) -> Option<(Vec<bool>, f64, i64)> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    let layout = &msg.result.layout;
    let mine = pane_rect(layout, pane_id)?;
    let column = |rect: &Rect, ratio: f64| -> (i64, i64) {
        let divider = rect.x + (rect.width as f64 * ratio).round() as i64;
        (divider, rect.x + rect.width)
    };
    // Our own column is the right child of a split when a right-docked sidebar
    // is the tab's rightmost pane: that split sizes us, not the Agent. When our
    // pane IS the split's whole left child (a `[sidebar | viewer]` tab) the
    // split is our own boundary for the same reason.
    let holds_our_column =
        |rect: &Rect, ratio: f64| split_bounds_pane(rect, Some("right"), ratio, mine);
    let candidates: Vec<(&Rect, f64, Vec<bool>)> = layout
        .splits
        .iter()
        .filter(|s| s.direction.as_deref() == Some("right"))
        .filter_map(|s| {
            let rect = s.rect.as_ref()?;
            let ratio = s.ratio?;
            let path = split_path(s.id.as_deref()?)?;
            (rect.width > 0 && !holds_our_column(rect, ratio)).then_some((rect, ratio, path))
        })
        .collect();
    // The rightmost column boundary: no other right split sits inside this
    // split's right child, or the Agent boundary itself would be skipped for a
    // nested one.
    let mut kept = Vec::with_capacity(candidates.len());
    for (index, (rect, ratio, _)) in candidates.iter().enumerate() {
        let (from, to) = column(rect, *ratio);
        let nested = candidates.iter().enumerate().any(|(other_index, other)| {
            other_index != index
                && other.0.width < rect.width
                && other.0.x >= from
                && other.0.x + other.0.width <= to
        });
        if !nested {
            kept.push(candidates[index].clone());
        }
    }
    kept.into_iter()
        .map(|(rect, ratio, path)| (rect.width * rect.height, path, ratio, rect.width))
        // Largest first; on equal area the SMALLER path wins, so the choice
        // never depends on the order herdr listed the splits in.
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
        .map(|(_, path, ratio, width)| (path, ratio, width))
}

/// One geometry-independent description of a split: what it separates and how.
/// Used to carry a tab's ratios across a dock repair, which moves panes through
/// a temporary tab and makes herdr re-create those splits at DEFAULT ratios —
/// see [`restore_split_ratios`].
#[derive(Clone, Debug, PartialEq)]
pub struct SplitRatio {
    pub direction: String,
    /// The ids of every pane this split's rect contains, SORTED, so the same
    /// split can be found again after panes moved around.
    pub panes: Vec<String>,
    pub ratio: f64,
}

/// The ids of the panes a split's rect contains, sorted (see [`SplitRatio`]).
/// The ids of every pane a split's rect contains, SORTED so the same split can
/// be recognised after panes moved. `ignore` drops one pane id — a dock restore
/// must compare against the pre-dock set, which cannot contain the sidebar that
/// is being docked.
fn contained_panes(layout: &Layout, rect: &Rect, ignore: Option<&str>) -> Vec<String> {
    let mut panes: Vec<String> = layout
        .panes
        .iter()
        .filter(|p| p.pane_id.as_deref() != ignore)
        .filter(|p| p.rect.as_ref().is_some_and(|r| rect_holds(rect, r)))
        .filter_map(|p| p.pane_id.clone())
        .collect();
    panes.sort();
    panes
}

/// True when `mine` IS one of this split's two children along the split's axis —
/// i.e. the split is our own column's boundary, which the sidebar launchers set
/// from `sidebar_width`. A split that merely CONTAINS our pane is not our
/// boundary: in the common `[[sidebar|editor] | agent]` shape the tab's root
/// split holds the sidebar but its ratio sizes the Agent column.
fn split_bounds_pane(rect: &Rect, direction: Option<&str>, ratio: f64, mine: &Rect) -> bool {
    let vertical = direction == Some("down");
    let (start, len, cross_start, cross_len) = if vertical {
        (rect.y, rect.height, rect.x, rect.width)
    } else {
        (rect.x, rect.width, rect.y, rect.height)
    };
    let (mine_start, mine_len, mine_cross, mine_cross_len) = if vertical {
        (mine.y, mine.height, mine.x, mine.width)
    } else {
        (mine.x, mine.width, mine.y, mine.height)
    };
    if cross_start > mine_cross || mine_cross + mine_cross_len > cross_start + cross_len {
        return false;
    }
    let divider = start + (len as f64 * ratio).round() as i64;
    (mine_start == start && mine_start + mine_len == divider)
        || (mine_start >= divider && mine_start + mine_len <= start + len)
}

/// Every split of a `pane.layout` reply, described by the SORTED pane ids its
/// rect contains (so the same split can be found again after panes move), the
/// direction, and its ratio. Splits without a rect/ratio/direction are skipped,
/// as are splits holding NO pane at all (a degenerate rect cannot describe a
/// boundary, and could never match a real split on the other side). Unparseable
/// input yields nothing rather than guessing.
pub fn split_ratios(layout_json: &str) -> Vec<SplitRatio> {
    let Ok(msg) = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)) else {
        return Vec::new();
    };
    let layout = &msg.result.layout;
    layout
        .splits
        .iter()
        .filter_map(|split| {
            let rect = split.rect.as_ref()?;
            let panes = contained_panes(layout, rect, None);
            Some(SplitRatio {
                direction: split.direction.clone()?,
                panes: (!panes.is_empty()).then_some(panes)?,
                ratio: split.ratio?,
            })
        })
        .collect()
}

/// `(path, ratio)` calls that put `before`'s ratios back into `after`.
///
/// A split in `after` matches when its direction and its contained-pane set are
/// identical to a `before` entry and the ratio differs by more than 1e-6. The
/// pane set is what identifies it across a repair (the split's path changes when
/// its parent is re-created), and it also stops a ratio from being applied to a
/// different split that happens to sit at the same path. Splits of `after` whose
/// rect contains `skip_pane` (the freshly docked sidebar) are never touched —
/// the sidebar's own column belongs to `sidebar_width`, and the tab's root split
/// must not be resized here.
pub fn restore_split_ratios(
    before: &[SplitRatio],
    after: &str,
    skip_pane: &str,
) -> Vec<(Vec<bool>, f64)> {
    let Ok(msg) = serde_json::from_str::<LayoutMsg>(strip_bom(after)) else {
        return Vec::new();
    };
    let layout = &msg.result.layout;
    let mine = pane_rect(layout, skip_pane);
    let mut calls = Vec::new();
    for split in &layout.splits {
        let (Some(rect), Some(ratio), Some(direction)) =
            (split.rect.as_ref(), split.ratio, split.direction.as_deref())
        else {
            continue;
        };
        // A split that IS the sidebar's own boundary keeps the launcher's
        // width; every other split's ratio is the user's layout, which the
        // dock repair just clobbered.
        if let Some(mine) = mine
            && split_bounds_pane(rect, Some(direction), ratio, mine)
        {
            continue;
        }
        let Some(path) = split.id.as_deref().and_then(split_path) else {
            continue;
        };
        let panes = contained_panes(layout, rect, Some(skip_pane));
        let Some(want) = before
            .iter()
            .find(|b| b.direction == direction && b.panes == panes)
        else {
            continue;
        };
        if (want.ratio - ratio).abs() > 1e-6 {
            calls.push((path, want.ratio));
        }
    }
    calls
}

/// The rect of `pane_id` in a decoded layout, if the pane is there.
fn pane_rect<'a>(layout: &'a Layout, pane_id: &str) -> Option<&'a Rect> {
    layout
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()
}

/// True when `rect` fully contains `inner` — i.e. a split at `rect` holds the
/// `inner` pane.
fn rect_holds(rect: &Rect, inner: &Rect) -> bool {
    rect.x <= inner.x
        && inner.x + inner.width <= rect.x + rect.width
        && rect.y <= inner.y
        && inner.y + inner.height <= rect.y + rect.height
}

/// The `down` split ratio that leaves the BOTTOM pane `rows` rows tall, out of
/// `split_height` rows: the ratio is the top child's share, so it is
/// `1 - rows / height`. Clamped so both sides keep at least one row and the
/// ratio stays inside herdr's own 0.1..0.9 bounds.
pub fn ratio_for_rows(rows: u16, split_height: i64) -> f64 {
    ratio_for_tail(rows, split_height)
}

/// The `right` split ratio that leaves the RIGHTMOST pane `cols` columns wide,
/// out of a `split_width`-wide split: the ratio is the left child's share, so
/// it is `1 - cols / width`. Clamped so both sides keep at least one column and
/// the ratio stays inside herdr's own 0.1..0.9 bounds.
pub fn ratio_for_cols(cols: u16, split_width: i64) -> f64 {
    ratio_for_tail(cols, split_width)
}

/// `1 - units / total` for the tail pane of a split, whichever axis it runs
/// along: the one implementation behind [`ratio_for_rows`] and
/// [`ratio_for_cols`].
fn ratio_for_tail(units: u16, total: i64) -> f64 {
    let total = total as f64;
    if total < 3.0 {
        // No room for a ratio that gives both panes a cell.
        return 0.5;
    }
    let units = f64::from(units).max(1.0).min(total - 1.0);
    (1.0 - units / total).clamp(0.1, 0.9)
}

/// True when the id can be passed as a positional argument to the herdr CLI
/// without any risk of being parsed as a flag.
fn is_flag_safe(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
}

fn strip_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strict toggle rewrites only the FOCUS decision; OPEN, CLOSE, and
    /// REPLACE (and garbage) must reach the launchers untouched.
    #[test]
    fn strict_toggle_maps_focus_to_close_and_nothing_else() {
        assert_eq!(focus_as_close("FOCUS w4:p2"), "CLOSE w4:p2");
        assert_eq!(focus_as_close("OPEN"), "OPEN");
        assert_eq!(focus_as_close("CLOSE w4:p2"), "CLOSE w4:p2");
        assert_eq!(focus_as_close("REPLACE w4:p2"), "REPLACE w4:p2");
        assert_eq!(focus_as_close(""), "");
    }

    /// The decision and the dock must reason about the SAME tab. Scoping only
    /// the spawn cwd would let the decision see a focused tab with no sidebar,
    /// answer OPEN, and dock a second sidebar into a scoped tab that already
    /// had one.
    #[test]
    fn the_decision_follows_the_same_scope_as_the_dock() {
        let json = pane_list(
            r#"{"pane_id":"w4:pM","tab_id":"w4:tY","workspace_id":"w4","focused":true,"cwd":"/repo/faultline"},
               {"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","cwd":"/repo/tremor"},
               {"pane_id":"wH:p2","tab_id":"wH:t1","workspace_id":"wH","label":"Explorer",
                "tokens":{"herdr-sidebar-explorer":"9999999999"}}"#,
        );
        // Unscoped: the focused tab has no explorer, so a fresh one is right.
        assert_eq!(launch_decision_in(&json, 9999999999, ""), "OPEN");
        // Scoped to wH, whose tab ALREADY has one — docking again would twin it.
        assert_eq!(launch_decision_in(&json, 9999999999, "wH"), "FOCUS wH:p2");
    }

    /// The sidebar is rooted from the cwd it is spawned with, so picking the
    /// globally focused pane roots a new tab's sidebar in whatever project
    /// the user was last looking at — observed live as tremor's sidebar
    /// rooted in bedrock. The event payload names the scope; honor it.
    #[test]
    fn spawn_cwd_comes_from_the_events_own_scope() {
        let json = pane_list(
            r#"{"pane_id":"w4:pM","tab_id":"w4:tY","workspace_id":"w4","focused":true,"foreground_cwd":"/repo/faultline"},
               {"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","foreground_cwd":"/repo/tremor"},
               {"pane_id":"wH:p9","tab_id":"wH:t2","workspace_id":"wH","foreground_cwd":"/repo/tremor/sub"}"#,
        );
        // A multi-tab workspace is ambiguous and must not borrow one tab.
        assert_eq!(focused_pane_in(&json, "wH"), "");
        // A tab scope is more specific still.
        assert_eq!(focused_pane_in(&json, "wH:t2"), "wH:p9\t/repo/tremor/sub");
        // No scope keeps the old global behavior.
        assert_eq!(focused_pane_in(&json, ""), "w4:pM\t/repo/faultline");
        // A scope whose panes are gone yields nothing rather than guessing.
        assert_eq!(focused_pane_in(&json, "wQ"), "");
    }

    /// Within a scope the focused pane still wins; a brand-new space may have
    /// no focused pane yet, and then any pane in it is the right root.
    #[test]
    fn a_scope_prefers_its_focused_pane_but_settles_for_any() {
        let json = pane_list(
            r#"{"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","foreground_cwd":"/repo/a"},
               {"pane_id":"wH:p2","tab_id":"wH:t1","workspace_id":"wH","focused":true,"foreground_cwd":"/repo/b"}"#,
        );
        assert_eq!(focused_pane_in(&json, "wH"), "wH:p2\t/repo/b");

        let unfocused = pane_list(
            r#"{"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","foreground_cwd":"/repo/a"}"#,
        );
        assert_eq!(focused_pane_in(&unfocused, "wH"), "wH:p1\t/repo/a");
    }

    #[test]
    fn spawn_scope_falls_back_to_cwd_when_live_cwd_is_unavailable() {
        let json = pane_list(
            r#"{"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","focused":true,
                "cwd":"C:\\repo\\tremor"}"#,
        );
        assert_eq!(focused_pane_in(&json, "wH:t1"), "wH:p1\tC:\\repo\\tremor");
    }

    #[test]
    fn event_scope_prefers_the_tab_then_the_workspace() {
        let tab = r#"{"event":"tab_focused","data":{"type":"tab_focused","tab_id":"w4:tY","workspace_id":"w4"}}"#;
        assert_eq!(event_scope(tab), "w4:tY");
        let ws = r#"{"event":"workspace_created","data":{"type":"workspace_created","workspace_id":"wH"}}"#;
        assert_eq!(event_scope(ws), "wH");
        // Nested workspace object, as workspace_created carries WorkspaceInfo.
        let nested = r#"{"event":"workspace_created","data":{"type":"workspace_created","workspace":{"workspace_id":"wH"}}}"#;
        assert_eq!(event_scope(nested), "wH");
        assert_eq!(event_scope("garbage"), "");
        // Shell-unsafe ids are dropped, same as the event kind.
        assert_eq!(
            event_scope(r#"{"data":{"workspace_id":"a b; rm -rf /"}}"#),
            ""
        );
    }

    #[test]
    fn pane_focus_scope_resolves_to_its_own_tab() {
        let event = r#"{"data":{"type":"pane_focused","pane_id":"w4:p9","workspace_id":"w4"}}"#;
        let panes = pane_list(
            r#"{"pane_id":"w4:p1","tab_id":"w4:t1","workspace_id":"w4"},
               {"pane_id":"w4:p9","tab_id":"w4:t2","workspace_id":"w4"}"#,
        );
        assert_eq!(event_scope_in(event, &panes), "w4:t2");
        assert_eq!(event_scope(event), "w4");
    }

    #[test]
    fn workspace_focus_uses_its_active_tab_context() {
        let event = r#"{"event":"workspace.focused","data":{"workspace_id":"w4"}}"#;
        let panes = pane_list(
            r#"{"pane_id":"w4:p1","tab_id":"w4:t1","workspace_id":"w4"},
               {"pane_id":"w4:p9","tab_id":"w4:t2","workspace_id":"w4","focused":true,"foreground_cwd":"/repo/two"}"#,
        );
        let scope = event_scope_with_tab_context(event, &panes, "w4:t2");
        assert_eq!(scope, "w4:t2");
        assert_eq!(focused_pane_in(&panes, &scope), "w4:p9\t/repo/two");
        assert_eq!(event_scope_with_tab_context(event, &panes, "w9:t1"), "w4");
    }

    /// The launcher handles several event kinds through one implementation, so the only
    /// way to treat space creation specially is the payload. The envelope
    /// shape isn't documented, so the discriminator is looked for at the top
    /// level and under the usual wrappers.
    #[test]
    fn event_kind_is_found_whatever_the_envelope() {
        assert_eq!(
            event_kind(r#"{"type":"workspace_created"}"#),
            "workspace_created"
        );
        assert_eq!(
            event_kind(r#"{"data":{"type":"workspace_created"}}"#),
            "workspace_created"
        );
        assert_eq!(
            event_kind(r#"{"event":{"type":"tab_focused"}}"#),
            "tab_focused"
        );
        assert_eq!(event_kind(r#"{"event":"tab.created"}"#), "tab_created");
    }

    /// The kind is interpolated into a shell command, so anything that is not
    /// a plain lower_snake identifier is dropped rather than passed along.
    #[test]
    fn event_kind_refuses_anything_shell_unsafe() {
        for hostile in [
            r#"{"type":"a; rm -rf /"}"#,
            r#"{"type":"$(whoami)"}"#,
            r#"{"type":"a b"}"#,
            r#"{"type":42}"#,
            r#"{"type":""}"#,
            "garbage",
        ] {
            assert_eq!(event_kind(hostile), "", "{hostile}");
        }
    }

    #[test]
    fn workspace_labels_resolve_by_id_and_degrade_quietly() {
        let json = r#"{"result":{"workspaces":[
            {"workspace_id":"wG","label":"tremor"},
            {"workspace_id":"w4","label":"faultline"},
            {"workspace_id":"wZ"}
        ]}}"#;
        assert_eq!(workspace_label(json, "wG"), "tremor");
        assert_eq!(workspace_label(json, "w4"), "faultline");
        // A space with no label, an id that is gone, and a missing id all
        // yield "" so the caller falls back to the pane's cwd.
        assert_eq!(workspace_label(json, "wZ"), "");
        assert_eq!(workspace_label(json, "wQ"), "");
        assert_eq!(workspace_label(json, ""), "");
        assert_eq!(workspace_label("garbage", "wG"), "");
    }

    /// herdr labels a new workspace's first tab with its position, and only that
    /// generated label is ours to replace.
    #[test]
    fn first_tab_renames_the_generated_label_only() {
        assert_eq!(
            first_tab_rename(
                r#"{"id":"cli:tab:get","result":{"tab":{"agent_status":"idle","focused":true,"label":"1","number":1,"tab_id":"wD:t1","workspace_id":"wD"},"type":"tab_info"}}"#
            ),
            Some(("wD:t1".to_string(), FIRST_TAB_LABEL))
        );
        // A later tab keeps its own number, even though its label matches it.
        assert_eq!(
            first_tab_rename(r#"{"result":{"tab":{"label":"2","number":2,"tab_id":"wD:t2"}}}"#),
            None
        );
        // A human's name, and our own rename, are never overwritten.
        assert_eq!(
            first_tab_rename(
                r#"{"result":{"tab":{"label":"tremor","number":1,"tab_id":"wD:t1"}}}"#
            ),
            None
        );
        assert_eq!(
            first_tab_rename(r#"{"result":{"tab":{"label":"main","number":1,"tab_id":"wD:t1"}}}"#),
            None
        );
    }

    /// A missing field or an unparsable reply means "nothing to rename": the
    /// hook must never guess at a tab id.
    #[test]
    fn first_tab_rename_degrades_quietly() {
        for hostile in [
            r#"{"result":{"tab":{"label":"1","number":1}}}"#,
            r#"{"result":{"tab":{"number":1,"tab_id":"wD:t1"}}}"#,
            r#"{"result":{"tab":{"label":"1","tab_id":"wD:t1"}}}"#,
            r#"{"result":{"tab":{"label":"1","number":1,"tab_id":""}}}"#,
            r#"{"result":{}}"#,
            r#"{"result":{"tab":{"label":"1","number":"1","tab_id":"wD:t1"}}}"#,
            r#"{"result":null}"#,
            "null",
            r#"{"error":{"code":"x","message":"y"}}"#,
            "garbage",
        ] {
            assert_eq!(first_tab_rename(hostile), None, "{hostile}");
        }
    }

    /// `workspace_created` names a workspace but no tab, so the generated first
    /// tab is found in that workspace's tab list — never borrowed from another
    /// workspace or from a tab somebody already named.
    #[test]
    fn generated_first_tab_in_picks_only_the_untouched_first_tab() {
        let list = r#"{"result":{"tabs":[
            {"label":"9","number":9,"tab_id":"wX:t9","workspace_id":"wX"},
            {"label":"1","number":1,"tab_id":"wD:t1","workspace_id":"wD"},
            {"label":"2","number":2,"tab_id":"wD:t2","workspace_id":"wD"}
        ]}}"#;
        assert_eq!(
            generated_first_tab_in(list, "wD"),
            Some("wD:t1".to_string())
        );

        // A human's name on the first tab is theirs to keep.
        assert_eq!(
            generated_first_tab_in(
                r#"{"result":{"tabs":[{"label":"tremor","number":1,"tab_id":"wD:t1","workspace_id":"wD"}]}}"#,
                "wD"
            ),
            None
        );
        // Only the first tab, never a later numbered one.
        assert_eq!(
            generated_first_tab_in(
                r#"{"result":{"tabs":[{"label":"1","number":1,"tab_id":"wD:t1","workspace_id":"wD"}]}}"#,
                "wE"
            ),
            None,
            "another workspace's tabs are not ours to rename"
        );
        assert_eq!(
            generated_first_tab_in(
                r#"{"result":{"tabs":[{"label":"2","number":2,"tab_id":"wD:t2","workspace_id":"wD"}]}}"#,
                "wD"
            ),
            None
        );

        // No workspace to look in, and replies we do not understand.
        for hostile in ["", r#"{"result":{"tabs":[]}}"#, "garbage", "null"] {
            assert_eq!(generated_first_tab_in(hostile, "wD"), None, "{hostile}");
        }
        assert_eq!(generated_first_tab_in(list, ""), None);
    }

    fn pane_list(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:list","result":{{"panes":[{panes}]}}}}"#)
    }

    #[test]
    fn cwd_follower_uses_only_live_foreground_cwd() {
        let mut follower = CwdFollower::default();
        let live = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar","tokens":{"herdr-sidebar-explorer":"1"}},
               {"pane_id":"w1:p1","tab_id":"w1:t1","cwd":"/stale/spawn","foreground_cwd":"/live/project"}"#,
        );
        assert_eq!(
            follower
                .next_cwd(&live, "w1:p3", Path::new("/elsewhere"))
                .as_deref(),
            Some("/live/project")
        );

        follower.reset();
        let stale_only = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","cwd":"/stale/spawn"}"#,
        );
        assert_eq!(
            follower.next_cwd(&stale_only, "w1:p3", Path::new("/elsewhere")),
            None
        );
    }

    #[test]
    fn cwd_follower_has_deterministic_multi_pane_precedence() {
        let root = Path::new("/elsewhere");
        let mut follower = CwdFollower::default();
        let unordered = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p9","tab_id":"w1:t1","foreground_cwd":"/nine"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"}"#,
        );
        // First sample, nothing at the root: lexical pane id is the fallback.
        assert_eq!(
            follower.next_cwd(&unordered, "w1:p3", root).as_deref(),
            Some("/one")
        );
        assert_eq!(follower.next_cwd(&unordered, "w1:p3", root), None);

        // Focus alone moves to p9 without any cwd change: no re-root.
        let focused = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p9","tab_id":"w1:t1","foreground_cwd":"/nine","focused":true},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"}"#,
        );
        assert_eq!(follower.next_cwd(&focused, "w1:p3", root), None);

        // Both change; the focused changed pane wins over the unfocused one.
        let both_changed = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p9","tab_id":"w1:t1","foreground_cwd":"/nine-2","focused":true},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one-2"}"#,
        );
        assert_eq!(
            follower.next_cwd(&both_changed, "w1:p3", root).as_deref(),
            Some("/nine-2")
        );

        // Both change again; now p1 is focused and wins.
        let both_changed_again = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p9","tab_id":"w1:t1","foreground_cwd":"/nine-3"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one-3","focused":true}"#,
        );
        assert_eq!(
            follower
                .next_cwd(&both_changed_again, "w1:p3", root)
                .as_deref(),
            Some("/one-3")
        );
    }

    #[test]
    fn manual_folder_wins_until_an_observed_pane_moves() {
        let root = Path::new("/elsewhere");
        let mut follower = CwdFollower::default();
        let initial = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","foreground_cwd":"/two"}"#,
        );
        assert_eq!(
            follower.next_cwd(&initial, "w1:p3", root).as_deref(),
            Some("/one")
        );
        follower.mark_manual_folder();

        // Focus alone, pane-list reordering, and a newly-created pane do not
        // override an explicit folder choice.
        let focus_only = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p4","tab_id":"w1:t1","foreground_cwd":"/new"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","foreground_cwd":"/two","focused":true},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"}"#,
        );
        assert_eq!(follower.next_cwd(&focus_only, "w1:p3", root), None);

        let moved = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","foreground_cwd":"/two/next","focused":true}"#,
        );
        assert_eq!(
            follower.next_cwd(&moved, "w1:p3", root).as_deref(),
            Some("/two/next")
        );

        // A manual choice made before the first sample adopts nothing.
        let mut fresh = CwdFollower::default();
        fresh.mark_manual_folder();
        assert_eq!(fresh.next_cwd(&initial, "w1:p3", root), None);
        assert_eq!(fresh.next_cwd(&focus_only, "w1:p3", root), None);
    }

    #[test]
    fn cwd_follower_ignores_plugin_panes_and_other_tabs() {
        let json = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","label":"Source Control","foreground_cwd":"/sc"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","label":"Preview · routes.rs","foreground_cwd":"/preview"},
               {"pane_id":"w1:p5","tab_id":"w1:t1","label":"routes.rs · editor","foreground_cwd":"/editor"},
               {"pane_id":"w1:p4","tab_id":"w1:t2","foreground_cwd":"/other"}"#,
        );
        assert_eq!(
            CwdFollower::default().next_cwd(&json, "w1:p3", Path::new("/elsewhere")),
            None
        );
    }

    #[test]
    fn focus_moving_to_an_unchanged_pane_never_re_roots() {
        // The IDE layout: Fresh and Agent stay at the project root while the
        // Terminal is the pane the user `cd`s in.
        let root = Path::new("/proj");
        let mut follower = CwdFollower::default();
        let list = |terminal: &str,
                    terminal_focused: bool,
                    fresh_focused: bool,
                    agent: &str,
                    agent_focused: bool| {
            pane_list(&format!(
                r#"{{"pane_id":"w1:p0","tab_id":"w1:t1","label":"Sidebar"}},
                   {{"pane_id":"w1:p4","tab_id":"w1:t1","label":"Fresh","foreground_cwd":"/proj","focused":{fresh_focused}}},
                   {{"pane_id":"w1:p1","tab_id":"w1:t1","label":"Terminal","foreground_cwd":"{terminal}","focused":{terminal_focused}}},
                   {{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Agent","foreground_cwd":"{agent}","focused":{agent_focused}}}"#
            ))
        };
        assert_eq!(
            follower.next_cwd(&list("/proj", false, false, "/proj", false), "w1:p0", root),
            None
        );
        assert_eq!(
            follower
                .next_cwd(
                    &list("/proj/sub", true, false, "/proj", false),
                    "w1:p0",
                    root
                )
                .as_deref(),
            Some("/proj/sub")
        );
        assert_eq!(
            follower.next_cwd(
                &list("/proj/sub", false, true, "/proj", false),
                "w1:p0",
                root
            ),
            None
        );
        assert_eq!(
            follower.next_cwd(
                &list("/proj/sub", false, false, "/proj", true),
                "w1:p0",
                root
            ),
            None
        );
        assert_eq!(
            follower
                .next_cwd(
                    &list("/proj/sub", false, false, "/other", true),
                    "w1:p0",
                    root
                )
                .as_deref(),
            Some("/other")
        );
    }

    #[test]
    fn first_sample_keeps_a_sibling_already_at_the_current_root() {
        let root = Path::new("/proj/sub");
        let mut follower = CwdFollower::default();
        let first = pane_list(
            r#"{"pane_id":"w1:p0","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p4","tab_id":"w1:t1","label":"Fresh","foreground_cwd":"/proj","focused":true},
               {"pane_id":"w1:p1","tab_id":"w1:t1","label":"Terminal","foreground_cwd":"/proj/sub"}"#,
        );
        assert_eq!(follower.next_cwd(&first, "w1:p0", root), None);

        let focus_moved = pane_list(
            r#"{"pane_id":"w1:p0","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p4","tab_id":"w1:t1","label":"Fresh","foreground_cwd":"/proj"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","label":"Terminal","foreground_cwd":"/proj/sub","focused":true}"#,
        );
        assert_eq!(follower.next_cwd(&focus_moved, "w1:p0", root), None);
    }

    const FOCUSED: &str =
        r#"{"pane_id":"w1:p1","focused":true,"tab_id":"w1:t1","cwd":"C:\\work\\my repo"}"#;

    #[test]
    fn parking_tab_is_detected_only_when_every_pane_is_parked() {
        let parked = |id: &str, tab: &str| {
            format!(
                r#"{{"pane_id":"{id}","tab_id":"{tab}","label":"Terminal","tokens":{{"ide-parked-for":"w1:t1"}}}}"#
            )
        };
        // Tab scope: the whole tab is parked.
        let all = pane_list(&format!("{},{}", parked("w1:p1", "w1:t2"), parked("w1:p2", "w1:t2")));
        assert!(in_parking_tab(&all, "w1:t2"));
        // Workspace scope: the focused pane decides the tab.
        let focused = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t2","workspace_id":"w1","focused":true,"tokens":{"ide-parked-for":"w1:t1"}},
               {"pane_id":"w1:p9","tab_id":"w1:t3","workspace_id":"w1"}"#,
        );
        assert!(in_parking_tab(&focused, "w1"));
        // A non-parked pane sharing the tab means it is a real tab.
        let mixed = pane_list(&format!(
            "{},{}",
            parked("w1:p1", "w1:t2"),
            r#"{"pane_id":"w1:p2","tab_id":"w1:t2","label":"Sidebar"}"#
        ));
        assert!(!in_parking_tab(&mixed, "w1:t2"));
        // An empty token value does not count as parked.
        let empty_value = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t2","tokens":{"ide-parked-for":""}}"#,
        );
        assert!(!in_parking_tab(&empty_value, "w1:t2"));
        // Garbage, empty lists and unknown scopes are never a parking tab.
        for hostile in ["", "garbage", "null", r#"{"result":{"panes":[]}}"#] {
            assert!(!in_parking_tab(hostile, ""), "{hostile}");
        }
        assert!(!in_parking_tab(&all, "w1:t9"));
    }

    #[test]
    fn follower_follows_a_terminal_parked_for_our_tab() {
        let root = Path::new("/proj/a");
        let mut follower = CwdFollower::default();
        let parked = |cwd: &str| {
            pane_list(&format!(
                r#"{{"pane_id":"w1:p0","tab_id":"w1:t1","label":"Sidebar"}},
                   {{"pane_id":"w1:p1","tab_id":"w1:t2","label":"Terminal","foreground_cwd":"{cwd}","tokens":{{"ide-parked-for":"w1:t1"}}}}"#
            ))
        };
        assert_eq!(follower.next_cwd(&parked("/proj/a"), "w1:p0", root), None);
        assert_eq!(
            follower
                .next_cwd(&parked("/proj/b"), "w1:p0", root)
                .as_deref(),
            Some("/proj/b")
        );
    }

    #[test]
    fn follower_ignores_a_terminal_parked_for_another_tab() {
        let json = pane_list(
            r#"{"pane_id":"w1:p0","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t2","label":"Terminal","foreground_cwd":"/other","tokens":{"ide-parked-for":"w1:t9"}}"#,
        );
        assert_eq!(
            CwdFollower::default().next_cwd(&json, "w1:p0", Path::new("/elsewhere")),
            None
        );
    }

    #[test]
    fn parking_a_terminal_does_not_re_root_our_tab() {
        let root = Path::new("/proj/a");
        let mut follower = CwdFollower::default();
        let home = pane_list(
            r#"{"pane_id":"w1:p0","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","label":"Terminal","foreground_cwd":"/proj/a"}"#,
        );
        assert_eq!(follower.next_cwd(&home, "w1:p0", root), None);
        // Same pane id and cwd, now parked in a background tab.
        let parked = pane_list(
            r#"{"pane_id":"w1:p0","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t2","label":"Terminal","foreground_cwd":"/proj/a","tokens":{"ide-parked-for":"w1:t1"}}"#,
        );
        assert_eq!(follower.next_cwd(&parked, "w1:p0", root), None);
        assert_eq!(follower.next_cwd(&parked, "w1:p0", root), None);
    }

    #[test]
    fn decision_opens_when_no_explorer_in_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","label":"Explorer","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(
            launch_decision(&json, 100),
            "OPEN",
            "other-tab Explorer is ignored"
        );
    }

    #[test]
    fn decision_focuses_unfocused_explorer_in_same_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","label":"Explorer","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":95}}}}"#
        ));
        assert_eq!(launch_decision(&json, 100), "FOCUS w1:p2");
    }

    #[test]
    fn decision_closes_when_explorer_is_focused() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","label":"Explorer","tab_id":"w1:t1","focused":true,"tokens":{"herdr-sidebar-explorer":95}}"#,
        );
        assert_eq!(launch_decision(&json, 100), "CLOSE w1:p2");
    }

    #[test]
    fn decision_recognizes_explorer_by_metadata_token_without_label() {
        // A collapsed explorer has its label cleared but keeps its token;
        // the token value is a fresh heartbeat timestamp.
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":95}}}}"#
        ));
        assert_eq!(launch_decision(&json, 100), "FOCUS w1:p2");
    }

    #[test]
    fn pane_token_probe_distinguishes_starting_and_live_sidebars() {
        let starting = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t1","label":"Explorer","focused":true,"tokens":{"herdr-sidebar-explorer":"100","herdr-sidebar-starting":"1"}}"#,
        );
        let live = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t1","label":"Explorer","tokens":{"herdr-sidebar-explorer":"100"}}"#,
        );
        // Synchronous pre-stamping makes the pane live to re-entrant hooks,
        // while retaining enough state for an immediate second toggle to
        // close it directly before its event loop starts.
        assert_eq!(launch_decision(&starting, 100), "CLOSE w1:p1");
        assert!(pane_has_token(&starting, "w1:p1"));
        assert!(pane_has_token(&live, "w1:p1"));
        assert!(!pane_has_token(&live, "w1:p2"));
        assert!(pane_is_starting(&starting, "w1:p1"));
        assert!(!pane_is_starting(&live, "w1:p1"));
        assert!(!pane_is_starting(&starting, "w1:p2"));

        let source_control = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t1","label":"Source Control","focused":true,"tokens":{"herdr-sidebar-git":"100","herdr-sidebar-starting":"1"}}"#,
        );
        assert_eq!(launch_decision_git(&source_control, 100), "CLOSE w1:p1");
        assert!(pane_is_starting(&source_control, "w1:p1"));
    }

    #[test]
    fn decision_replaces_dead_panes() {
        // Stale heartbeat (or a pre-heartbeat token shape) = a dead TUI whose
        // pane must be closed and re-docked, never focused.
        let stale = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":40}}}}"#
        ));
        assert_eq!(launch_decision(&stale, 100), "REPLACE w1:p2");
        let legacy = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":{{"value":"explorer"}}}}}}"#
        ));
        assert_eq!(launch_decision(&legacy, 100), "REPLACE w1:p2");
        // Label without a token = a RESUMED corpse (labels survive server
        // restarts, tokens and the process do not) — for every our-label.
        for label in ["Explorer", "Sidebar"] {
            let corpse = pane_list(&format!(
                r#"{FOCUSED},{{"pane_id":"w1:p2","label":"{label}","tab_id":"w1:t1"}}"#
            ));
            assert_eq!(
                launch_decision(&corpse, 100),
                "REPLACE w1:p2",
                "{label} corpse"
            );
        }
        let sc_corpse = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p3","label":"Source Control","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision_git(&sc_corpse, 100), "REPLACE w1:p3");
        // Same rules for the separated Source Control decision.
        let sc_stale = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p3","tab_id":"w1:t1","tokens":{{"herdr-sidebar-git":40}}}}"#
        ));
        assert_eq!(launch_decision_git(&sc_stale, 100), "REPLACE w1:p3");
        let sc_live = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p3","tab_id":"w1:t1","tokens":{{"herdr-sidebar-git":95}}}}"#
        ));
        assert_eq!(launch_decision_git(&sc_live, 100), "FOCUS w1:p3");
    }

    #[test]
    fn decision_degrades_to_open_on_garbage_or_unsafe_ids() {
        assert_eq!(launch_decision("not json", 100), "OPEN");
        assert_eq!(
            launch_decision(&pane_list(r#"{"pane_id":"w1:p1"}"#), 100),
            "OPEN"
        );
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"--evil","label":"Explorer","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&json, 100), "OPEN");
    }

    #[test]
    fn utf8_bom_from_powershell_pipe_is_stripped() {
        let json = format!("\u{feff}{}", pane_list(FOCUSED));
        assert_eq!(launch_decision(&json, 100), "OPEN");
        assert!(focused_pane(&json).starts_with("w1:p1\t"));
        let layout_json = format!(
            "\u{feff}{}",
            layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":90,"height":50}}"#)
        );
        assert_eq!(open_plan(&layout_json, false, 32), "w1:p1\t0.355556\ttrue");
    }

    #[test]
    fn focused_pane_reports_id_and_stripped_cwd() {
        let json = pane_list(&format!(
            r#"{{"pane_id":"w1:p3","focused":true,"tab_id":"w1:t1","cwd":"\\\\?\\C:\\work\\my repo"}},{FOCUSED}"#
        ));
        assert_eq!(focused_pane(&json), "w1:p3\tC:\\work\\my repo");
        assert_eq!(focused_pane("not json"), "");
        assert_eq!(focused_pane(&pane_list(r#"{"pane_id":"w1:p1"}"#)), "");
    }

    fn layout(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:layout","result":{{"layout":{{"panes":[{panes}]}}}}}}"#)
    }

    #[test]
    fn open_plan_picks_leftmost_topmost_pane() {
        let json = layout(
            r#"{"pane_id":"w1:p2","rect":{"x":119,"y":1,"width":90,"height":70}},
               {"pane_id":"w1:p3","rect":{"x":29,"y":36,"width":90,"height":35}},
               {"pane_id":"w1:p1","rect":{"x":29,"y":1,"width":90,"height":35}}"#,
        );
        let plan = open_plan(&json, false, 32);
        let (id, ratio) = plan.split_once('\t').unwrap();
        assert_eq!(id, "w1:p1");
        assert_eq!(ratio, "0.355556\ttrue"); // 32 / 90, then swap left
    }

    #[test]
    fn open_plan_picks_rightmost_topmost_pane_and_inverts_ratio() {
        let json = layout(
            r#"{"pane_id":"w1:p2","rect":{"x":119,"y":36,"width":90,"height":35}},
               {"pane_id":"w1:p3","rect":{"x":29,"y":1,"width":90,"height":70}},
               {"pane_id":"w1:p1","rect":{"x":119,"y":1,"width":90,"height":35}}"#,
        );
        assert_eq!(open_plan(&json, true, 32), "w1:p1\t0.644444\tfalse");
    }

    #[test]
    fn open_plan_clamps_ratio() {
        let wide = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":400,"height":50}}"#);
        assert_eq!(open_plan(&wide, false, 32), "w1:p1\t0.150000\ttrue");
        assert_eq!(open_plan(&wide, true, 32), "w1:p1\t0.850000\tfalse");
        let narrow = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":40,"height":50}}"#);
        assert_eq!(open_plan(&narrow, false, 32), "w1:p1\t0.500000\ttrue");
        assert_eq!(open_plan(&narrow, true, 32), "w1:p1\t0.500000\tfalse");
        assert_eq!(open_plan(&narrow, false, 24), "w1:p1\t0.500000\ttrue");
    }

    #[test]
    fn layout_width_distinguishes_tab_area_from_pane_width() {
        let json = r#"{"result":{"layout":{"area":{"x":0,"y":0,"width":174,"height":50},"panes":[{"pane_id":"e","rect":{"x":0,"y":0,"width":42,"height":50}}]}}}"#;
        assert_eq!(layout_width(json), Some(174));
        assert_eq!(layout_width("not json"), None);
    }

    #[test]
    fn focused_tab_and_live_tabs() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(focused_tab(&json), "w1:t1");
        assert_eq!(focused_workspace(&json), "w1");
        assert_eq!(
            live_tabs(&json).into_iter().collect::<Vec<_>>(),
            vec!["w1:t1".to_string(), "w1:t2".to_string()]
        );
        assert_eq!(focused_tab("not json"), "");
        assert_eq!(focused_workspace("not json"), "");
        assert!(live_tabs("not json").is_empty());
    }

    #[test]
    fn split_pane_id_extracts_and_validates() {
        let json =
            r#"{"id":"x","result":{"pane":{"pane_id":"w3:p5","cwd":"C:x"},"type":"pane_info"}}"#;
        assert_eq!(split_pane_id(json), Some("w3:p5".to_string()));
        let evil = r#"{"id":"x","result":{"pane":{"pane_id":"--evil"},"type":"pane_info"}}"#;
        assert_eq!(split_pane_id(evil), None);
        assert_eq!(split_pane_id("not json"), None);
        assert_eq!(split_pane_id(r#"{"id":"x","result":{"type":"ok"}}"#), None);
    }

    #[test]
    fn plugin_pane_id_extracts_and_validates() {
        let json = r#"{"result":{"plugin_pane":{"plugin_id":"herdr-sidebar","entrypoint":"sidebar","pane":{"pane_id":"w3:p5"}}}}"#;
        assert_eq!(plugin_pane_id(json), Some("w3:p5".to_string()));
        let evil = r#"{"result":{"plugin_pane":{"pane":{"pane_id":"--evil"}}}}"#;
        assert_eq!(plugin_pane_id(evil), None);
        assert_eq!(plugin_pane_id("not json"), None);
    }

    fn layout_with_splits(panes: &str, splits: &str) -> String {
        format!(
            r#"{{"id":"cli:pane:layout","result":{{"layout":{{"panes":[{panes}],"splits":[{splits}]}}}}}}"#
        )
    }

    fn layout_with_area(panes: &str) -> String {
        format!(
            r#"{{"id":"x","result":{{"layout":{{"area":{{"x":0,"y":0,"width":180,"height":50}},"tab_id":"w1:t1","panes":[{panes}]}}}}}}"#
        )
    }

    /// A `pane.layout` snapshot with the tab area, the pane list, and one
    /// horizontal split spanning 160 columns whose divider sits at `ratio`.
    fn drag_snapshot(area_w: i64, panes: &str, ratio: f64, zoomed: bool) -> String {
        format!(
            r#"{{"result":{{"layout":{{"zoomed":{zoomed},"area":{{"x":0,"y":0,"width":{area_w},"height":50}},"panes":[{panes}],"splits":[{{"direction":"right","ratio":{ratio:.6},"rect":{{"x":0,"y":0,"width":160,"height":50}}}}]}}}}}}"#
        )
    }

    /// A left-docked sidebar `side_w` columns wide beside one working pane.
    fn drag_panes(side_w: i64) -> String {
        let rest = 160 - side_w;
        format!(
            r#"{{"pane_id":"e","rect":{{"x":0,"y":0,"width":{side_w},"height":50}}}},{{"pane_id":"p1","rect":{{"x":{side_w},"y":0,"width":{rest},"height":50}}}}"#
        )
    }

    #[test]
    fn dragged_width_accepts_a_real_drag() {
        let before = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, false);
        let wider = drag_snapshot(160, &drag_panes(44), 44.0 / 160.0, false);
        assert_eq!(dragged_width(&before, &wider, "e", 42), Some(42));
        let narrower = drag_snapshot(160, &drag_panes(26), 26.0 / 160.0, false);
        assert_eq!(dragged_width(&wider, &narrower, "e", 24), Some(24));
    }

    #[test]
    fn dragged_width_rejects_a_window_resize() {
        let before = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, false);
        let after = drag_snapshot(170, &drag_panes(44), 44.0 / 170.0, false);
        assert_eq!(dragged_width(&before, &after, "e", 42), None);
    }

    #[test]
    fn dragged_width_rejects_a_changed_pane_set() {
        let before = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, false);
        // A split (or a preview pane) appeared between the two snapshots.
        let added = format!(
            r#"{},{{"pane_id":"p2","rect":{{"x":150,"y":0,"width":10,"height":50}}}}"#,
            drag_panes(44)
        );
        let after = drag_snapshot(160, &added, 44.0 / 160.0, false);
        assert_eq!(dragged_width(&before, &after, "e", 42), None);
        // …and a pane disappeared.
        assert_eq!(dragged_width(&after, &before, "e", 30), None);
    }

    #[test]
    fn dragged_width_rejects_zoom() {
        let plain = drag_snapshot(160, &drag_panes(44), 44.0 / 160.0, false);
        let zoomed = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, true);
        assert_eq!(dragged_width(&zoomed, &plain, "e", 42), None);
        let zoomed_after = drag_snapshot(160, &drag_panes(44), 44.0 / 160.0, true);
        assert_eq!(dragged_width(&plain, &zoomed_after, "e", 42), None);
    }

    #[test]
    fn dragged_width_rejects_an_unchanged_width() {
        let before = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, false);
        let after = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, false);
        assert_eq!(dragged_width(&before, &after, "e", 30), None);
    }

    #[test]
    fn dragged_width_rejects_widths_pinned_on_the_share_clamp() {
        // On a 160-column split the share bounds sit at 24 and 80 columns.
        let before = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, false);
        let min = drag_snapshot(160, &drag_panes(24), 0.15, false);
        assert_eq!(dragged_width(&before, &min, "e", 22), None);
        let max = drag_snapshot(160, &drag_panes(80), 0.5, false);
        assert_eq!(dragged_width(&before, &max, "e", 78), None);
    }

    #[test]
    fn dragged_width_rejects_garbage_unknown_pane_and_missing_split() {
        let before = drag_snapshot(160, &drag_panes(32), 32.0 / 160.0, false);
        let after = drag_snapshot(160, &drag_panes(44), 44.0 / 160.0, false);
        assert_eq!(dragged_width("not json", &after, "e", 42), None);
        assert_eq!(dragged_width(&before, "not json", "e", 42), None);
        assert_eq!(dragged_width(&before, &after, "", 42), None);
        assert_eq!(dragged_width(&before, &after, "missing", 42), None);
        // Same area and pane set, but no horizontal split covers the pane.
        let splitless = format!(
            r#"{{"result":{{"layout":{{"area":{{"x":0,"y":0,"width":160,"height":50}},"panes":[{}]}}}}}}"#,
            drag_panes(44)
        );
        assert_eq!(dragged_width(&before, &splitless, "e", 42), None);
    }

    #[test]
    fn repair_step_finds_below_and_right_panes() {
        // Explorer truncated to the top-left; p7 spans below it, p1 beside it.
        let json = layout_with_area(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":32,"height":25}},
               {"pane_id":"p1","rect":{"x":32,"y":0,"width":58,"height":25}},
               {"pane_id":"p7","rect":{"x":0,"y":25,"width":90,"height":25}},
               {"pane_id":"p4","rect":{"x":90,"y":0,"width":90,"height":50}}"#,
        );
        let step = repair_step(&json, "e", false).unwrap();
        assert_eq!(
            (step.below.as_str(), step.beside.as_str(), step.tab.as_str()),
            ("p7", "p1", "w1:t1")
        );
    }

    #[test]
    fn repair_step_mirrors_for_right_dock() {
        let json = layout_with_area(
            r#"{"pane_id":"p4","rect":{"x":0,"y":0,"width":90,"height":50}},
               {"pane_id":"p1","rect":{"x":90,"y":0,"width":58,"height":25}},
               {"pane_id":"e","rect":{"x":148,"y":0,"width":32,"height":25}},
               {"pane_id":"p7","rect":{"x":90,"y":25,"width":90,"height":25}}"#,
        );
        let step = repair_step(&json, "e", true).unwrap();
        assert_eq!(
            (step.below.as_str(), step.beside.as_str(), step.tab.as_str()),
            ("p7", "p1", "w1:t1")
        );
    }

    #[test]
    fn repair_step_none_when_full_height_or_unmatched() {
        let full = layout_with_area(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":32,"height":50}},
               {"pane_id":"p1","rect":{"x":32,"y":0,"width":148,"height":50}}"#,
        );
        assert!(repair_step(&full, "e", false).is_none());
        assert!(repair_step(&full, "missing", false).is_none());
        assert!(repair_step("not json", "e", false).is_none());
    }

    #[test]
    fn resize_plan_converts_columns_to_split_ratio_delta() {
        // Explorer: 32 rect cols (30 terminal cols) at the left of a 160-col split.
        let json = layout_with_splits(
            r#"{"pane_id":"w1:p2","rect":{"x":0,"y":0,"width":32,"height":50}},
               {"pane_id":"w1:p1","rect":{"x":32,"y":0,"width":128,"height":50}}"#,
            r#"{"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        // Collapse 30 → 4 terminal cols: rect 32 → 6, delta -26/160.
        let step = resize_plan(&json, "w1:p2", 30, 4, false).unwrap();
        assert_eq!(step.direction, "left");
        assert!((step.amount - 26.0 / 160.0).abs() < 1e-9);
        // Expand 30 → 40: rect 32 → 42, delta +10/160.
        let step = resize_plan(&json, "w1:p2", 30, 40, false).unwrap();
        assert_eq!(step.direction, "right");
        assert!((step.amount - 10.0 / 160.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_mirrors_directions_for_right_dock() {
        let json = layout_with_splits(
            r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":128,"height":50}},
               {"pane_id":"w1:p2","rect":{"x":128,"y":0,"width":32,"height":50}}"#,
            r#"{"direction":"right","ratio":0.8,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        let shrink = resize_plan(&json, "w1:p2", 30, 4, true).unwrap();
        assert_eq!(shrink.direction, "right");
        assert!((shrink.amount - 26.0 / 160.0).abs() < 1e-9);
        let grow = resize_plan(&json, "w1:p2", 30, 40, true).unwrap();
        assert_eq!(grow.direction, "left");
        assert!((grow.amount - 10.0 / 160.0).abs() < 1e-9);
    }

    #[test]
    fn preferred_resize_plan_pins_columns_but_yields_at_extremes() {
        let normal = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":39,"height":50}}"#,
            r#"{"direction":"right","ratio":0.195,"rect":{"x":0,"y":0,"width":200,"height":50}}"#,
        );
        let step = preferred_resize_plan(&normal, "e", 37, 42, false).unwrap();
        assert_eq!(step.direction, "right");
        assert!((step.amount - 5.0 / 200.0).abs() < 1e-9);

        let narrow = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":20,"height":50}}"#,
            r#"{"direction":"right","ratio":0.333333,"rect":{"x":0,"y":0,"width":60,"height":50}}"#,
        );
        let step = preferred_resize_plan(&narrow, "e", 18, 42, false).unwrap();
        assert_eq!(step.direction, "right");
        assert!((step.amount - 10.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_picks_innermost_matching_split() {
        // Nested: root split (divider elsewhere) plus the inner split whose
        // divider is at the explorer's right edge.
        let json = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":20,"height":50}}"#,
            r#"{"direction":"right","ratio":0.5,"rect":{"x":0,"y":0,"width":200,"height":50}},
               {"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":100,"height":50}}"#,
        );
        let step = resize_plan(&json, "e", 18, 40, false).unwrap();
        // delta computed against the inner 100-col split: +22/100.
        assert!((step.amount - 22.0 / 100.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_returns_none_when_unresizable_or_at_target() {
        assert!(resize_plan("not json", "e", 30, 4, false).is_none());
        // No split with a divider at the pane's edge (e.g. the only pane).
        let solo = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":100,"height":50}}"#,
            "",
        );
        assert!(resize_plan(&solo, "e", 98, 30, false).is_none());
        // Already at the target.
        let json = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert!(resize_plan(&json, "e", 30, 30, false).is_none());
    }

    #[test]
    fn open_plan_is_empty_on_failure() {
        assert_eq!(open_plan("not json", false, 32), "");
        assert_eq!(open_plan(&layout(""), false, 32), "");
        let unsafe_id = layout(r#"{"pane_id":"--x","rect":{"x":0,"y":0,"width":90,"height":50}}"#);
        assert_eq!(open_plan(&unsafe_id, true, 32), "");
    }

    /// The IDE tab is `[sidebar | [[fresh|terminal] | agent]]`. The editor over
    /// terminal split is the only one a "terminal height" can size, and the
    /// sidebar's own column is not it.
    #[test]
    fn foreign_down_split_finds_the_editor_terminal_column() {
        let json = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}},
               {"pane_id":"fresh","rect":{"x":32,"y":0,"width":80,"height":35}},
               {"pane_id":"term","rect":{"x":32,"y":35,"width":80,"height":15}},
               {"pane_id":"agent","rect":{"x":112,"y":0,"width":48,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}},
               {"id":"split_2_1","direction":"down","ratio":0.7,"rect":{"x":32,"y":0,"width":80,"height":50}},
               {"id":"split_3_11","direction":"right","ratio":0.625,"rect":{"x":32,"y":0,"width":128,"height":50}}"#,
        );
        // The sidebar is in its own column, so the editor/terminal split is
        // foreign to it: that is the one a terminal height sizes.
        assert_eq!(
            foreign_down_split(&json, "side"),
            Some((vec![true], 0.7, 50))
        );
        // A pane INSIDE the split is "ours" for that split, so it is skipped
        // (sizing the split we live in is not what this helper is for).
        assert_eq!(foreign_down_split(&json, "fresh"), None);
        assert_eq!(foreign_down_split(&json, "term"), None);
        // Another column of the same tab finds the same foreign split.
        assert_eq!(
            foreign_down_split(&json, "agent"),
            Some((vec![true], 0.7, 50))
        );
    }

    #[test]
    fn foreign_down_split_skips_a_split_holding_our_own_pane() {
        // A sidebar stacked over a preview: the down split IS our column, so
        // there is nothing here for a terminal height to size.
        let json = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":30}},
               {"pane_id":"viewer","rect":{"x":0,"y":30,"width":32,"height":20}}"#,
            r#"{"id":"split_1_root","direction":"down","ratio":0.6,"rect":{"x":0,"y":0,"width":32,"height":50}}"#,
        );
        assert_eq!(foreign_down_split(&json, "side"), None);
        assert_eq!(foreign_down_split(&json, "viewer"), None);
    }

    #[test]
    fn foreign_down_split_prefers_the_innermost_then_the_path() {
        // Two qualifying down splits (a full-height column and a nested half),
        // plus a right split that must be ignored.
        let json = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":20,"height":50}},
               {"pane_id":"a","rect":{"x":20,"y":0,"width":70,"height":30}},
               {"pane_id":"b","rect":{"x":20,"y":30,"width":70,"height":20}}"#,
            r#"{"id":"split_9_0","direction":"down","ratio":0.5,"rect":{"x":20,"y":0,"width":140,"height":50}},
               {"id":"split_4_01","direction":"down","ratio":0.4,"rect":{"x":20,"y":0,"width":70,"height":40}},
               {"id":"split_2_1","direction":"right","ratio":0.5,"rect":{"x":20,"y":0,"width":140,"height":50}}"#,
        );
        assert_eq!(
            foreign_down_split(&json, "side"),
            Some((vec![false, true], 0.4, 40))
        );
    }

    #[test]
    fn foreign_down_split_needs_an_addressable_split() {
        let rights_only = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert_eq!(foreign_down_split(&rights_only, "side"), None);
        // A host that does not encode the path in the split id (pre-0.9) is
        // unaddressable: claim nothing rather than guess a wrong path.
        let id_less = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"direction":"down","ratio":0.7,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert_eq!(foreign_down_split(&id_less, "side"), None);
        // Garbage, an unknown pane, and a split without a rect or ratio.
        assert_eq!(foreign_down_split("not json", "side"), None);
        assert_eq!(foreign_down_split(&rights_only, "missing"), None);
        let partial = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"down","rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert_eq!(foreign_down_split(&partial, "side"), None);
    }

    /// The IDE tab is `[sidebar | [[fresh|terminal] | agent]]`. The Agent
    /// column is the tab's right split that does not hold the sidebar itself,
    /// so an Agent width can find it from either dock side.
    #[test]
    fn foreign_right_split_finds_the_agent_column() {
        let json = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}},
               {"pane_id":"fresh","rect":{"x":32,"y":0,"width":80,"height":35}},
               {"pane_id":"term","rect":{"x":32,"y":35,"width":80,"height":15}},
               {"pane_id":"agent","rect":{"x":112,"y":0,"width":48,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}},
               {"id":"split_2_1","direction":"down","ratio":0.7,"rect":{"x":32,"y":0,"width":80,"height":50}},
               {"id":"split_3_11","direction":"right","ratio":0.625,"rect":{"x":32,"y":0,"width":128,"height":50}}"#,
        );
        // The root split is the sidebar's own boundary (its left child IS our
        // column), so the Agent column is the next right split.
        assert_eq!(
            foreign_right_split(&json, "side"),
            Some((vec![true, true], 0.625, 128))
        );
        // A pane inside that boundary (the editor column) is its left child, so
        // nothing there sizes an Agent column.
        assert_eq!(foreign_right_split(&json, "fresh"), None);
    }

    /// The layout shape herdr actually docks into (verified live): the sidebar
    /// is NESTED in the same left subtree as the editor, so the Agent boundary
    /// is the tab's ROOT split and its rect does contain our pane. Rejecting
    /// candidates by containment alone made this shape unsizeable.
    #[test]
    fn foreign_right_split_finds_a_root_agent_boundary_holding_our_column() {
        let json = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":42}},
               {"pane_id":"fresh","rect":{"x":32,"y":0,"width":99,"height":30}},
               {"pane_id":"term","rect":{"x":32,"y":30,"width":99,"height":12}},
               {"pane_id":"agent","rect":{"x":131,"y":0,"width":88,"height":42}}"#,
            r#"{"id":"split_0_root","direction":"right","ratio":0.6,"rect":{"x":0,"y":0,"width":219,"height":42}},
               {"id":"split_1_0","direction":"right","ratio":0.243538,"rect":{"x":0,"y":0,"width":131,"height":42}},
               {"id":"split_2_01","direction":"down","ratio":0.714286,"rect":{"x":32,"y":0,"width":99,"height":42}}"#,
        );
        assert_eq!(foreign_right_split(&json, "side"), Some((vec![], 0.6, 219)));
    }

    #[test]
    fn foreign_right_split_finds_the_agent_column_with_a_right_docked_sidebar() {
        let json = layout_with_splits(
            r#"{"pane_id":"fresh","rect":{"x":0,"y":0,"width":80,"height":35}},
               {"pane_id":"term","rect":{"x":0,"y":35,"width":80,"height":15}},
               {"pane_id":"agent","rect":{"x":80,"y":0,"width":48,"height":50}},
               {"pane_id":"side","rect":{"x":128,"y":0,"width":32,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.8,"rect":{"x":0,"y":0,"width":160,"height":50}},
               {"id":"split_2_0","direction":"right","ratio":0.625,"rect":{"x":0,"y":0,"width":128,"height":50}},
               {"id":"split_3_1","direction":"down","ratio":0.7,"rect":{"x":0,"y":0,"width":80,"height":50}}"#,
        );
        // The sidebar's own split is skipped, so its docking side is irrelevant.
        assert_eq!(
            foreign_right_split(&json, "side"),
            Some((vec![false], 0.625, 128))
        );
    }

    #[test]
    fn foreign_right_split_skips_a_split_holding_our_own_pane() {
        // A sidebar stacked over a preview: both right splits here hold our
        // column, so there is nothing for an Agent width to size.
        let json = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":30}},
               {"pane_id":"viewer","rect":{"x":0,"y":30,"width":32,"height":20}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert_eq!(foreign_right_split(&json, "side"), None);
        assert_eq!(foreign_right_split(&json, "viewer"), None);
    }

    #[test]
    fn foreign_right_split_prefers_the_largest_then_the_path() {
        // Two qualifying right splits: a nested one inside the middle column
        // and the outer middle|agent boundary. The Agent column is the OUTER
        // one, so the larger rect wins whatever order herdr lists them in.
        let json = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":20,"height":50}},
               {"pane_id":"a","rect":{"x":20,"y":0,"width":60,"height":50}},
               {"pane_id":"agent","rect":{"x":80,"y":0,"width":80,"height":50}}"#,
            r#"{"id":"split_4_1","direction":"right","ratio":0.5,"rect":{"x":20,"y":0,"width":60,"height":50}},
               {"id":"split_3_11","direction":"right","ratio":0.625,"rect":{"x":20,"y":0,"width":140,"height":50}}"#,
        );
        assert_eq!(
            foreign_right_split(&json, "side"),
            Some((vec![true, true], 0.625, 140))
        );
        // Equal areas fall back to the smaller path, so the choice does not
        // depend on the order of `.splits`.
        let tied = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":20,"height":50}},
               {"pane_id":"a","rect":{"x":20,"y":0,"width":140,"height":50}}"#,
            r#"{"id":"split_9_1","direction":"right","ratio":0.5,"rect":{"x":20,"y":0,"width":140,"height":50}},
               {"id":"split_3_0","direction":"right","ratio":0.7,"rect":{"x":20,"y":0,"width":140,"height":50}}"#,
        );
        assert_eq!(
            foreign_right_split(&tied, "side"),
            Some((vec![false], 0.7, 140))
        );
    }

    #[test]
    fn foreign_right_split_needs_an_addressable_split() {
        let downs_only = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"down","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert_eq!(foreign_right_split(&downs_only, "side"), None);
        // A host that does not encode the path in the split id (pre-0.9) is
        // unaddressable: claim nothing rather than guess a wrong path.
        let id_less = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"direction":"right","ratio":0.7,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert_eq!(foreign_right_split(&id_less, "side"), None);
        // Garbage, an unknown pane, and a split without a rect or ratio.
        assert_eq!(foreign_right_split("not json", "side"), None);
        assert_eq!(foreign_right_split(&downs_only, "missing"), None);
        let partial = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert_eq!(foreign_right_split(&partial, "side"), None);
    }

    #[test]
    fn ratio_for_cols_leaves_the_right_pane_that_many_columns() {
        // A 200-column anchor split so the Agent column keeps 40": 0.8 left.
        assert!((ratio_for_cols(40, 200) - 0.8).abs() < 1e-9);
        assert!((ratio_for_cols(160, 200) - 0.2).abs() < 1e-9);
        // herdr's own 0.1..0.9 clamp, mirrored here.
        assert!((ratio_for_cols(1, 200) - 0.9).abs() < 1e-9);
        assert!((ratio_for_cols(199, 200) - 0.1).abs() < 1e-9);
        // Both panes keep at least one column.
        assert!((ratio_for_cols(240, 200) - 0.1).abs() < 1e-9);
        // Too small to size at all: stay neutral instead of dividing by ~0.
        assert!((ratio_for_cols(40, 2) - 0.5).abs() < 1e-9);
        assert!((ratio_for_cols(40, 0) - 0.5).abs() < 1e-9);
        // Rows and columns share one implementation.
        assert_eq!(ratio_for_cols(10, 40), ratio_for_rows(10, 40));
    }

    /// The dock repair re-parents the pane under the sidebar, so herdr gives
    /// the re-created splits NEW paths and default ratios. The pane set is what
    /// identifies a split across that: the IDE's editor/terminal split must
    /// come back at the user's ratio however its path moved.
    #[test]
    fn restore_split_ratios_follows_a_split_whose_path_moved() {
        let panes = r#"{"pane_id":"term","rect":{"x":32,"y":35,"width":80,"height":15}},
               {"pane_id":"fresh","rect":{"x":32,"y":0,"width":80,"height":35}}"#;
        let before = layout_with_splits(
            panes,
            r#"{"id":"split_2_1","direction":"down","ratio":0.7,"rect":{"x":32,"y":0,"width":80,"height":50}}"#,
        );
        let after = layout_with_splits(
            panes,
            r#"{"id":"split_3_01","direction":"down","ratio":0.5,"rect":{"x":32,"y":0,"width":80,"height":50}}"#,
        );
        assert_eq!(
            restore_split_ratios(&split_ratios(&before), &after, "side"),
            vec![(vec![false, true], 0.7)]
        );
    }

    /// A split whose rect now holds a DIFFERENT set of panes is not the same
    /// split: applying a stale ratio to whatever moved there would resize the
    /// wrong boundary (the sidebar's own column, an Agent column's inner split).
    #[test]
    fn restore_split_ratios_skips_a_split_whose_pane_set_changed() {
        let before = layout_with_splits(
            r#"{"pane_id":"fresh","rect":{"x":32,"y":0,"width":80,"height":35}},
               {"pane_id":"term","rect":{"x":32,"y":35,"width":80,"height":15}}"#,
            r#"{"id":"split_2_1","direction":"down","ratio":0.7,"rect":{"x":32,"y":0,"width":80,"height":50}}"#,
        );
        // Same direction, path, and rect — but a pane appeared inside it.
        let after = layout_with_splits(
            r#"{"pane_id":"fresh","rect":{"x":32,"y":0,"width":80,"height":20}},
               {"pane_id":"term","rect":{"x":32,"y":20,"width":80,"height":10}},
               {"pane_id":"extra","rect":{"x":32,"y":30,"width":80,"height":20}}"#,
            r#"{"id":"split_2_1","direction":"down","ratio":0.5,"rect":{"x":32,"y":0,"width":80,"height":50}}"#,
        );
        assert!(restore_split_ratios(&split_ratios(&before), &after, "side").is_empty());
    }

    /// The freshly docked sidebar's own column is `sidebar_width`'s job, and the
    /// tab's root split holds it, so neither may be resized from a pre-dock
    /// snapshot (that snapshot predates the sidebar and had no such split).
    #[test]
    fn restore_split_ratios_skips_the_docked_sidebars_own_split() {
        // `[fresh | agent]` becomes `[sidebar | [fresh | agent]]`: the root split
        // is now the sidebar's own boundary (its first child IS our column), so it
        // must keep the launcher's width, while the split it wraps is restored.
        let before = layout_with_splits(
            r#"{"pane_id":"fresh","rect":{"x":0,"y":0,"width":60,"height":50}},
               {"pane_id":"agent","rect":{"x":60,"y":0,"width":20,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.75,"rect":{"x":0,"y":0,"width":80,"height":50}}"#,
        );
        let after = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":20,"height":50}},
               {"pane_id":"fresh","rect":{"x":20,"y":0,"width":45,"height":50}},
               {"pane_id":"agent","rect":{"x":65,"y":0,"width":15,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.25,"rect":{"x":0,"y":0,"width":80,"height":50}},
               {"id":"split_2_1","direction":"right","ratio":0.4,"rect":{"x":20,"y":0,"width":60,"height":50}}"#,
        );
        assert_eq!(
            restore_split_ratios(&split_ratios(&before), &after, "side"),
            vec![(vec![true], 0.75)]
        );
    }

    /// The real IDE shape (verified live): `[[sidebar|editor] | agent]`, where the
    /// Agent boundary is the tab's ROOT split and its rect therefore CONTAINS the
    /// sidebar. A dock restore has to put both the Agent ratio and the
    /// editor/terminal height back; skipping every split that holds our column (the
    /// first version of this) restored only the height and silently dropped the
    /// Agent width.
    #[test]
    fn restore_split_ratios_restores_a_whole_ide_tab_around_a_docked_sidebar() {
        // Before the dock: no sidebar; Agent 122 of 338, terminal 17 of 69.
        let before = layout_with_splits(
            r#"{"pane_id":"fresh","rect":{"x":0,"y":0,"width":216,"height":52}},
               {"pane_id":"term","rect":{"x":0,"y":52,"width":216,"height":17}},
               {"pane_id":"agent","rect":{"x":216,"y":0,"width":122,"height":69}}"#,
            r#"{"id":"split_0_root","direction":"right","ratio":0.639053,"rect":{"x":0,"y":0,"width":338,"height":69}},
               {"id":"split_2_01","direction":"down","ratio":0.753623,"rect":{"x":0,"y":0,"width":216,"height":69}}"#,
        );
        // After the dock + full_height_repair: the repair recreated both splits at
        // herdr's defaults (0.6 / 0.5) and added the sidebar's own split.
        let after = layout_with_splits(
            r#"{"pane_id":"side","rect":{"x":0,"y":0,"width":32,"height":69}},
               {"pane_id":"fresh","rect":{"x":32,"y":0,"width":171,"height":34}},
               {"pane_id":"term","rect":{"x":32,"y":34,"width":171,"height":35}},
               {"pane_id":"agent","rect":{"x":203,"y":0,"width":135,"height":69}}"#,
            r#"{"id":"split_0_root","direction":"right","ratio":0.6,"rect":{"x":0,"y":0,"width":338,"height":69}},
               {"id":"split_1_0","direction":"right","ratio":0.157635,"rect":{"x":0,"y":0,"width":203,"height":69}},
               {"id":"split_2_01","direction":"down","ratio":0.5,"rect":{"x":32,"y":0,"width":171,"height":69}}"#,
        );
        assert_eq!(
            restore_split_ratios(&split_ratios(&before), &after, "side"),
            vec![(Vec::new(), 0.639053), (vec![false, true], 0.753623)]
        );
    }

    /// Only a real difference is a call: a split the repair left alone must not
    /// be written back (every write is an IPC round trip and a layout event).
    #[test]
    fn restore_split_ratios_says_nothing_when_the_ratio_is_unchanged() {
        let panes = r#"{"pane_id":"fresh","rect":{"x":32,"y":0,"width":80,"height":35}},
               {"pane_id":"term","rect":{"x":32,"y":35,"width":80,"height":15}}"#;
        let before = layout_with_splits(
            panes,
            r#"{"id":"split_2_1","direction":"down","ratio":0.7,"rect":{"x":32,"y":0,"width":80,"height":50}}"#,
        );
        let after = layout_with_splits(
            panes,
            r#"{"id":"split_3_01","direction":"down","ratio":0.7,"rect":{"x":32,"y":0,"width":80,"height":50}}"#,
        );
        assert!(restore_split_ratios(&split_ratios(&before), &after, "side").is_empty());
        // Unreadable input never produces a guess.
        assert!(restore_split_ratios(&[], "not json", "side").is_empty());
        assert!(restore_split_ratios(&split_ratios("not json"), &after, "side").is_empty());
    }

    /// The snapshot itself: one entry per split that actually describes a
    /// boundary, panes SORTED so the key does not depend on the order herdr
    /// lists them in. A split with no ratio, or one whose rect holds no pane,
    /// contributes nothing.
    #[test]
    fn split_ratios_describes_every_addressable_split() {
        let json = layout_with_splits(
            r#"{"pane_id":"term","rect":{"x":32,"y":35,"width":80,"height":15}},
               {"pane_id":"fresh","rect":{"x":32,"y":0,"width":80,"height":35}},
               {"pane_id":"agent","rect":{"x":112,"y":0,"width":48,"height":50}}"#,
            r#"{"id":"split_1_root","direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}},
               {"id":"split_2_1","direction":"down","ratio":0.7,"rect":{"x":32,"y":0,"width":80,"height":50}},
               {"direction":"right","ratio":0.9,"rect":{"x":0,"y":0,"width":1,"height":1}},
               {"id":"split_3_0","direction":"right","rect":{"x":0,"y":0,"width":1,"height":1}}"#,
        );
        assert_eq!(
            split_ratios(&json),
            vec![
                SplitRatio {
                    direction: "right".into(),
                    panes: vec!["agent".into(), "fresh".into(), "term".into()],
                    ratio: 0.2,
                },
                SplitRatio {
                    direction: "down".into(),
                    panes: vec!["fresh".into(), "term".into()],
                    ratio: 0.7,
                },
            ]
        );
        assert!(split_ratios("not json").is_empty());
    }

    #[test]
    fn ratio_for_rows_leaves_the_bottom_pane_that_many_rows() {
        // A 40-row column with a 10-row terminal leaves 30 rows on top: 0.75.
        assert!((ratio_for_rows(10, 40) - 0.75).abs() < 1e-9);
        // herdr's own 0.1..0.9 clamp, mirrored here.
        assert!((ratio_for_rows(4, 40) - 0.9).abs() < 1e-9);
        assert!((ratio_for_rows(39, 40) - 0.1).abs() < 1e-9);
        // Both panes keep at least one row.
        assert!((ratio_for_rows(40, 40) - 0.1).abs() < 1e-9);
        assert!((ratio_for_rows(0, 40) - 0.9).abs() < 1e-9);
        // Too small to size at all: stay neutral instead of dividing by ~0.
        assert!((ratio_for_rows(4, 2) - 0.5).abs() < 1e-9);
        assert!((ratio_for_rows(4, 0) - 0.5).abs() < 1e-9);
    }
}
