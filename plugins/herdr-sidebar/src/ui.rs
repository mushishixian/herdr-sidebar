//! Shared UI vocabulary for both sidebar views: keycap footer hints, the
//! activity-bar / settings / suggest glyphs, hit-test helpers, and the
//! pane-list parsing both views use to find their sibling panes. One home —
//! these used to be copy-mirrored between two crates.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::sync::atomic::{AtomicU8, Ordering};

use crate::icons::IconTheme;
use crate::state::{ColorTheme, View};

/// Keycap chip colors for the footer hints — a subtle "keyboard key" look
/// instead of a wall of dim text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub keycap_bg: Color,
    pub keycap_fg: Color,
    pub modified: Color,
    pub untracked: Color,
    pub added: Color,
    pub renamed: Color,
    pub deleted: Color,
    pub conflict: Color,
    pub ignored: Color,
    pub selection_bg: Color,
    pub selection_unfocused_bg: Color,
    /// Full-size activity-bar hover: visibly related to selection, but weaker.
    pub activity_hover_bg: Color,
    pub hover_bg: Color,
    pub accent: Color,
    pub accent_focus: Color,
    /// Filled buttons (✓ Commit, the section count badge). The dark themes
    /// fill them with the accent; the light theme uses a soft tint with dark
    /// blue text, because a saturated blue block dominates a white pane.
    pub button_bg: Color,
    pub button_focus_bg: Color,
    pub button_fg: Color,
    pub muted_button_bg: Color,
    pub muted_button_fg: Color,
    pub sync_bg: Color,
    pub sync_busy_bg: Color,
    /// Sync Changes' label. It sits on `sync_bg`, NOT on the accent, so it
    /// cannot borrow `button_fg` — white on a light grey button is invisible.
    pub sync_fg: Color,
    pub header_accent: Color,
    /// Row foregrounds that go with the three row backgrounds above.
    /// `Color::Reset` = keep the terminal's own foreground.
    pub selection_fg: Color,
    pub selection_unfocused_fg: Color,
    pub hover_fg: Color,
    /// Mouse text selection in the preview / editor. Unlike the list rows this
    /// sits UNDER syntax-colored spans, so it must not fight their foreground.
    pub text_selection_bg: Color,
    /// Diff row tints (`diffview`), word-level tint, and the +/− gutter marks.
    pub diff_del_bg: Color,
    pub diff_del_word_bg: Color,
    pub diff_add_bg: Color,
    pub diff_add_word_bg: Color,
    pub diff_del_mark: Color,
    pub diff_add_mark: Color,
    /// Advisory text (the editor's "experimental" tag, the font installer's
    /// manual command) — `Yellow` is illegible on a light background.
    pub warning: Color,
}

// VS Code's dark-theme git decoration colors, shared by the Source Control
// rows and the Explorer's status decorations (issue #19) so one status letter
// always means one color.
const VSCODE_PALETTE: Palette = Palette {
    keycap_bg: Color::Rgb(0x32, 0x36, 0x3d),
    keycap_fg: Color::Rgb(0xc9, 0xce, 0xd6),
    modified: Color::Rgb(0xe2, 0xc0, 0x8d),
    untracked: Color::Rgb(0x73, 0xc9, 0x91),
    added: Color::Rgb(0x81, 0xb8, 0x8b),
    renamed: Color::Rgb(0x73, 0xc9, 0x91),
    deleted: Color::Rgb(0xc7, 0x4e, 0x39),
    conflict: Color::Rgb(0xe4, 0x67, 0x6b),
    ignored: Color::Rgb(0x6b, 0x6b, 0x6b),
    selection_bg: Color::DarkGray,
    selection_unfocused_bg: Color::Rgb(0x2a, 0x2d, 0x2e),
    activity_hover_bg: Color::Rgb(0x3c, 0x42, 0x5d),
    hover_bg: Color::Rgb(48, 52, 60),
    accent: Color::Rgb(0x00, 0x78, 0xd4),
    accent_focus: Color::Rgb(0x02, 0x8a, 0xf0),
    button_bg: Color::Rgb(0x00, 0x78, 0xd4),
    button_focus_bg: Color::Rgb(0x02, 0x8a, 0xf0),
    button_fg: Color::White,
    muted_button_bg: Color::Rgb(0x24, 0x45, 0x5c),
    muted_button_fg: Color::Rgb(0x9a, 0xb2, 0xc2),
    sync_bg: Color::Rgb(0x3a, 0x3d, 0x41),
    sync_busy_bg: Color::Rgb(0x2d, 0x2d, 0x33),
    sync_fg: Color::White,
    header_accent: Color::LightBlue,
    selection_fg: Color::Reset,
    selection_unfocused_fg: Color::Reset,
    hover_fg: Color::Reset,
    text_selection_bg: Color::DarkGray,
    diff_del_bg: Color::Rgb(0x42, 0x22, 0x26),
    diff_del_word_bg: Color::Rgb(0x6f, 0x30, 0x36),
    diff_add_bg: Color::Rgb(0x20, 0x39, 0x28),
    diff_add_word_bg: Color::Rgb(0x35, 0x59, 0x3d),
    diff_del_mark: Color::Rgb(0xd1, 0x6d, 0x76),
    diff_add_mark: Color::Rgb(0x8c, 0xc9, 0x8f),
    warning: Color::Yellow,
};

// The same vocabulary drawn for a LIGHT terminal background: VS Code Light+
// git decorations, GitHub-light diff tints. Every value here is picked to stay
// readable on white — nothing may rely on a dark background showing through.
const LIGHT_PALETTE: Palette = Palette {
    keycap_bg: Color::Rgb(0xdd, 0xe1, 0xe6),
    keycap_fg: Color::Rgb(0x24, 0x29, 0x2f),
    modified: Color::Rgb(0x89, 0x55, 0x03),
    untracked: Color::Rgb(0x00, 0x71, 0x00),
    added: Color::Rgb(0x58, 0x7c, 0x0c),
    renamed: Color::Rgb(0x00, 0x71, 0x00),
    deleted: Color::Rgb(0xad, 0x07, 0x07),
    conflict: Color::Rgb(0xb5, 0x20, 0x0d),
    ignored: Color::Rgb(0x8c, 0x8c, 0x8c),
    selection_bg: Color::Rgb(0xcc, 0xe3, 0xf5),
    selection_unfocused_bg: Color::Rgb(0xe4, 0xe6, 0xe8),
    activity_hover_bg: Color::Rgb(0xe3, 0xef, 0xf9),
    hover_bg: Color::Rgb(0xec, 0xec, 0xec),
    accent: Color::Rgb(0x00, 0x78, 0xd4),
    accent_focus: Color::Rgb(0x02, 0x6e, 0xc1),
    // A soft fill with dark blue text instead of a solid blue block, and NEVER
    // `Color::White`: that is ANSI 15, which a light profile draws as a pale
    // grey — the old solid button read grey-on-blue.
    button_bg: Color::Rgb(0xd8, 0xea, 0xfc),
    button_focus_bg: Color::Rgb(0xb6, 0xd8, 0xf8),
    button_fg: Color::Rgb(0x0a, 0x4a, 0x86),
    // The inactive repo's button must stay clearly weaker than that tint.
    muted_button_bg: Color::Rgb(0xec, 0xef, 0xf2),
    muted_button_fg: Color::Rgb(0x57, 0x61, 0x6b),
    sync_bg: Color::Rgb(0xdc, 0xdf, 0xe3),
    sync_busy_bg: Color::Rgb(0xeb, 0xed, 0xef),
    sync_fg: Color::Rgb(0x24, 0x29, 0x2f),
    header_accent: Color::Rgb(0x00, 0x58, 0xa8),
    selection_fg: Color::Rgb(0x0a, 0x0a, 0x0a),
    selection_unfocused_fg: Color::Rgb(0x1f, 0x1f, 0x1f),
    hover_fg: Color::Reset,
    text_selection_bg: Color::Rgb(0xad, 0xd6, 0xff),
    diff_del_bg: Color::Rgb(0xff, 0xeb, 0xe9),
    diff_del_word_bg: Color::Rgb(0xff, 0xc1, 0xbc),
    diff_add_bg: Color::Rgb(0xe6, 0xff, 0xec),
    diff_add_word_bg: Color::Rgb(0xab, 0xf2, 0xbc),
    diff_del_mark: Color::Rgb(0xb3, 0x1d, 0x28),
    diff_add_mark: Color::Rgb(0x1a, 0x7f, 0x37),
    warning: Color::Rgb(0x9a, 0x67, 0x00),
};

const TERMINAL_PALETTE: Palette = Palette {
    keycap_bg: Color::DarkGray,
    keycap_fg: Color::White,
    modified: Color::Yellow,
    untracked: Color::Green,
    added: Color::LightGreen,
    renamed: Color::Green,
    deleted: Color::Red,
    conflict: Color::LightRed,
    ignored: Color::DarkGray,
    selection_bg: Color::DarkGray,
    selection_unfocused_bg: Color::Black,
    activity_hover_bg: Color::Rgb(0x3c, 0x42, 0x5d),
    hover_bg: Color::Black,
    accent: Color::Blue,
    accent_focus: Color::LightBlue,
    button_bg: Color::Blue,
    button_focus_bg: Color::LightBlue,
    button_fg: Color::White,
    muted_button_bg: Color::Black,
    muted_button_fg: Color::Gray,
    sync_bg: Color::DarkGray,
    sync_busy_bg: Color::Black,
    sync_fg: Color::White,
    header_accent: Color::LightBlue,
    selection_fg: Color::White,
    selection_unfocused_fg: Color::White,
    hover_fg: Color::Gray,
    text_selection_bg: Color::DarkGray,
    // Diff tints stay RGB in every dark theme: an ANSI background here would
    // collide with the syntax foregrounds the terminal profile also remaps.
    diff_del_bg: Color::Rgb(0x42, 0x22, 0x26),
    diff_del_word_bg: Color::Rgb(0x6f, 0x30, 0x36),
    diff_add_bg: Color::Rgb(0x20, 0x39, 0x28),
    diff_add_word_bg: Color::Rgb(0x35, 0x59, 0x3d),
    diff_del_mark: Color::Rgb(0xd1, 0x6d, 0x76),
    diff_add_mark: Color::Rgb(0x8c, 0xc9, 0x8f),
    warning: Color::Yellow,
};

/// 0 = vscode, 1 = terminal, 2 = light. The numbering is historical: this used
/// to hold a bool, and `0`/`1` are what an already-running pane reads.
static ACTIVE_PALETTE: AtomicU8 = AtomicU8::new(0);

fn theme_code(theme: ColorTheme) -> u8 {
    match theme {
        ColorTheme::VsCode => 0,
        ColorTheme::Terminal => 1,
        ColorTheme::Light => 2,
    }
}

pub fn set_color_theme(theme: ColorTheme) {
    ACTIVE_PALETTE.store(theme_code(theme), Ordering::Relaxed);
}

pub fn palette_for(theme: ColorTheme) -> Palette {
    match theme {
        ColorTheme::VsCode => VSCODE_PALETTE,
        ColorTheme::Light => LIGHT_PALETTE,
        ColorTheme::Terminal => TERMINAL_PALETTE,
    }
}

pub fn palette() -> Palette {
    palette_for(active_color_theme())
}

fn active_color_theme() -> ColorTheme {
    match ACTIVE_PALETTE.load(Ordering::Relaxed) {
        1 => ColorTheme::Terminal,
        2 => ColorTheme::Light,
        _ => ColorTheme::VsCode,
    }
}

/// The active palette assumes a light terminal background — the preview's
/// syntax theme and the icon colors branch on this.
pub fn is_light() -> bool {
    active_color_theme().is_light()
}

/// `Style::fg` only when the palette names a foreground; `Color::Reset` means
/// "leave the terminal's own", which is what the dark palettes want.
fn with_fg(style: Style, fg: Color) -> Style {
    if fg == Color::Reset {
        style
    } else {
        style.fg(fg)
    }
}

fn selection_style_for(theme: ColorTheme, focused: bool) -> Style {
    let colors = palette_for(theme);
    if focused {
        with_fg(
            Style::default().bg(colors.selection_bg),
            colors.selection_fg,
        )
        .add_modifier(Modifier::BOLD)
    } else {
        with_fg(
            Style::default().bg(colors.selection_unfocused_bg),
            colors.selection_unfocused_fg,
        )
    }
}

pub fn selection_style(focused: bool) -> Style {
    selection_style_for(active_color_theme(), focused)
}

fn hover_style_for(theme: ColorTheme) -> Style {
    let colors = palette_for(theme);
    with_fg(Style::default().bg(colors.hover_bg), colors.hover_fg)
}

pub fn hover_style() -> Style {
    hover_style_for(active_color_theme())
}

/// Idle/hover styling shared by compact chrome buttons. Title actions and
/// always-visible toolbars should look like one family rather than inventing
/// a separate filled-button treatment for each surface.
pub fn chrome_button_style(hovered: bool) -> Style {
    if hovered {
        Style::default()
            .bg(palette().keycap_bg)
            .fg(palette().keycap_fg)
    } else {
        Style::default().dim()
    }
}

/// Activity-bar buttons keep the stronger selected chip when active and use
/// the same subtle keycap hover as the title-bar actions when inactive.
pub fn activity_button_style(active: bool, hovered: bool) -> Style {
    if active {
        selection_style(true)
    } else if hovered {
        Style::default()
            .bg(palette().activity_hover_bg)
            .fg(palette().keycap_fg)
    } else {
        Style::default().dim()
    }
}

/// Extend an activity button's middle-row fill into its spacer rows with
/// half blocks. Hover and selection use the exact same three-row geometry.
pub fn draw_activity_caps(
    frame: &mut Frame,
    bounds: (u16, u16),
    outer_top: u16,
    outer_bottom: u16,
    color: Color,
) {
    let width = bounds.1.saturating_sub(bounds.0);
    if width == 0 {
        return;
    }
    let cap = |glyph: &str| {
        Paragraph::new(glyph.repeat(usize::from(width))).style(Style::default().fg(color))
    };
    frame.render_widget(cap("▄"), Rect::new(bounds.0, outer_top, width, 1));
    frame.render_widget(cap("▀"), Rect::new(bounds.0, outer_bottom, width, 1));
}

/// A file-type icon's fixed color, dimmed into legibility when the terminal
/// background is light. The icon table (`icons::material`) is tuned for a dark
/// pane — pale yellows and cyans vanish on white — so cap the relative
/// luminance instead of maintaining a second color table that would drift.
pub fn icon_style(rgb: Option<(u8, u8, u8)>) -> Style {
    icon_style_for(active_color_theme(), rgb)
}

fn icon_style_for(theme: ColorTheme, rgb: Option<(u8, u8, u8)>) -> Style {
    match rgb {
        Some(rgb) => {
            let (r, g, b) = if theme.is_light() {
                darken_for_light(rgb)
            } else {
                rgb
            };
            Style::default().fg(Color::Rgb(r, g, b))
        }
        None => Style::default(),
    }
}

/// Scale a color toward black until its relative luminance is at most
/// `MAX_LIGHT_LUMA`, preserving hue. Colors already dark enough pass through.
fn darken_for_light((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
    const MAX_LIGHT_LUMA: f32 = 0.42;
    let luma = (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0;
    if luma <= MAX_LIGHT_LUMA {
        return (r, g, b);
    }
    let scale = MAX_LIGHT_LUMA / luma;
    let dim = |c: u8| (f32::from(c) * scale).round().clamp(0.0, 255.0) as u8;
    (dim(r), dim(g), dim(b))
}

pub fn keep_visible_scroll(selected: usize, visible: usize, content: usize) -> usize {
    selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(content.saturating_sub(visible))
}

/// The color for a git status letter (`crate::git::FileEntry::letter`, plus
/// `I` for ignored). Shared vocabulary: the Explorer decorations and the
/// Source Control list must never disagree about what `M` looks like.
pub fn status_color(letter: char) -> Color {
    let colors = palette();
    match letter {
        'M' => colors.modified,
        'U' => colors.untracked,
        'A' => colors.added,
        'R' | 'C' => colors.renamed,
        'D' => colors.deleted,
        '!' => colors.conflict,
        'I' => colors.ignored,
        _ => Color::Reset,
    }
}

/// Rendered width of one `key label` hint: keycap padding + gap + label.
fn hint_width(key: &str, label: &str) -> usize {
    Span::raw(key).width() + 2 + 1 + Span::raw(label).width()
}

/// Pack hotkey hints into as many footer lines as they need at `width`
/// (max 4), instead of clipping — each as a keycap chip plus a dim label.
/// `reserve` columns stay free on the LAST line (for a corner button).
pub fn wrap_hints(
    hints: &[(&'static str, &'static str)],
    width: u16,
    reserve: u16,
) -> Vec<Line<'static>> {
    let colors = palette();
    let width = usize::from(width.max(8));
    let reserve = usize::from(reserve);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used: usize = 1;
    for (key, label) in hints {
        let w = hint_width(key, label);
        let empty = lines.last().is_some_and(Vec::is_empty);
        if !empty && used + 2 + w > width.saturating_sub(reserve) {
            lines.push(Vec::new());
            used = 1;
        }
        let line = lines.last_mut().unwrap();
        line.push(Span::raw(if line.is_empty() { " " } else { "  " }));
        line.push(Span::styled(
            format!(" {key} "),
            Style::default().bg(colors.keycap_bg).fg(colors.keycap_fg),
        ));
        line.push(Span::styled(format!(" {label}"), Style::default().dim()));
        used += if line.len() == 3 { w } else { 2 + w };
    }
    lines.into_iter().map(Line::from).collect()
}

/// A subtle right-edge scrollbar when the list overflows its viewport.
/// Purely an indicator: the wheel scrolls, the bar just shows where.
pub fn draw_scrollbar(frame: &mut Frame, area: Rect, total: usize, viewport: usize, pos: usize) {
    if total <= viewport || area.width == 0 || area.height == 0 {
        return;
    }
    let mut state = ScrollbarState::new(total.saturating_sub(viewport)).position(pos);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(Style::default().dim())
            .thumb_style(Style::default()),
        area,
        &mut state,
    );
}

/// True when a click at pane-local (column, row) lands on the `«` collapse
/// button: the 3-cell region at the right end of the bottom line, mirroring
/// herdr's own sidebar collapse control.
pub fn hits_collapse_button(column: u16, row: u16, pane_width: u16, pane_height: u16) -> bool {
    row == pane_height.saturating_sub(1) && column >= pane_width.saturating_sub(4)
}

/// Theme-matched activity-bar icons: (explorer, search, source control). FA
/// glyphs render two cells wide in the non-Mono Nerd Font — chips reserve
/// the second cell (see the activity-bar renderer).
pub fn activity_icons(theme: IconTheme) -> (&'static str, &'static str, &'static str) {
    match theme {
        IconTheme::Material => ("\u{f07b}", "\u{f002}", "\u{f126}"),
        IconTheme::Emoji => ("📁", "🔍", "🔀"),
    }
}

/// Theme-matched ⚙ settings glyph.
pub fn gear_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{f013}",
        IconTheme::Emoji => "⚙",
    }
}

/// Monochrome outline sparkles for the suggest button: MDI "creation"
/// (the classic three-sparkle ✨ silhouette) with a text-presentation
/// fallback for the emoji theme.
pub fn sparkle_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{f0674}",
        IconTheme::Emoji => "✧",
    }
}

/// Theme-matched branch glyph for repo rows.
pub fn branch_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{e725}",
        IconTheme::Emoji => "⎇",
    }
}

/// One VS Code-style title-bar action button (the hover row at the top-right
/// of a panel's title bar).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TitleAction {
    NewFile,
    NewFolder,
    Refresh,
    CollapseAll,
    ViewAsTree,
    ViewAsList,
}

/// How long the title-bar action buttons stay visible after the last mouse
/// event. Terminals emit no "mouse left the pane" event, so hover can only be
/// approximated: any mouse activity over the pane shows the buttons, and they
/// fade after this linger (any further motion re-shows them instantly).
pub const TITLE_ACTIONS_LINGER: std::time::Duration = std::time::Duration::from_secs(3);

/// The hover approximation described on [`TITLE_ACTIONS_LINGER`].
pub fn title_actions_visible(last_mouse: Option<std::time::Instant>) -> bool {
    last_mouse.is_some_and(|at| at.elapsed() < TITLE_ACTIONS_LINGER)
}

/// Theme-matched glyph for a title-bar action: VS Code's own codicons in the
/// material theme (the Nerd Font ships the cod- set), VS16-free fallbacks
/// otherwise.
pub fn title_action_icon(theme: IconTheme, action: TitleAction) -> &'static str {
    match (theme, action) {
        (IconTheme::Material, TitleAction::NewFile) => "\u{ea7f}", //  cod-new_file
        (IconTheme::Material, TitleAction::NewFolder) => "\u{ea80}", //  cod-new_folder
        (IconTheme::Material, TitleAction::Refresh) => "\u{eb37}", //  cod-refresh
        (IconTheme::Material, TitleAction::CollapseAll) => "\u{eac5}", //  cod-collapse_all
        (IconTheme::Material, TitleAction::ViewAsTree) => "\u{eb86}", //  cod-list_tree
        (IconTheme::Material, TitleAction::ViewAsList) => "\u{eb84}", //  cod-list_flat
        (IconTheme::Emoji, TitleAction::NewFile) => "📄",
        (IconTheme::Emoji, TitleAction::NewFolder) => "📁",
        (IconTheme::Emoji, TitleAction::Refresh) => "⟳",
        (IconTheme::Emoji, TitleAction::CollapseAll) => "⊟",
        (IconTheme::Emoji, TitleAction::ViewAsTree) => "🌲",
        (IconTheme::Emoji, TitleAction::ViewAsList) => "☰",
    }
}

/// A button's rendered chip: one space each side, NO extra slack cell. The
/// Mono Nerd Font build renders codicons in a single cell, so a trailing
/// slack cell (as the activity bar uses) pushes the glyph's center left of
/// the chip's — its right edge lands mid-chip (user-reported). In the
/// non-Mono build the glyph just overflows into its own trailing space, which
/// is how the tree's file icons already render.
fn title_action_chip(theme: IconTheme, action: TitleAction) -> String {
    format!(" {} ", title_action_icon(theme, action))
}

/// Total rendered width of the button row, for right-aligning it.
pub fn title_actions_width(theme: IconTheme, actions: &[TitleAction]) -> u16 {
    actions
        .iter()
        .map(|&a| Span::raw(title_action_chip(theme, a)).width() as u16)
        .sum()
}

/// Build the title-bar buttons as spans (left edge at `x` on row `y`) plus
/// their click zones for hit-testing. The chip under `hover` renders with a
/// keycap background so the mouse target is visible before the click.
pub fn title_action_spans(
    theme: IconTheme,
    actions: &[TitleAction],
    x: u16,
    y: u16,
    hover: Option<(u16, u16)>,
) -> (Vec<Span<'static>>, Vec<(Rect, TitleAction)>) {
    let mut spans = Vec::new();
    let mut zones = Vec::new();
    let mut cx = x;
    for &action in actions {
        let chip = title_action_chip(theme, action);
        let w = Span::raw(chip.as_str()).width() as u16;
        let rect = Rect::new(cx, y, w, 1);
        let style = chrome_button_style(hover.is_some_and(|(hx, hy)| hits(rect, hx, hy)));
        spans.push(Span::styled(chip, style));
        zones.push((rect, action));
        cx += w;
    }
    (spans, zones)
}

pub fn within(x: u16, (start, end): (u16, u16)) -> bool {
    (start..end).contains(&x)
}

pub fn hits_activity_button((start, end): (u16, u16), middle_row: u16, x: u16, y: u16) -> bool {
    within(x, (start, end))
        && (middle_row.saturating_sub(1)..=middle_row.saturating_add(1)).contains(&y)
}

pub fn hits(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Cut `s` down to at most `max` display columns, ending in `…` when trimmed.
/// Empty when even the ellipsis wouldn't fit.
/// Word-wrap a uniform-style footer message to the pane width: one leading
/// space per line, `reserve` columns kept clear of the right edge (the «
/// button zone). Unbreakable words longer than a line hard-break. Never
/// returns an empty vec — footers size themselves from `.len()`.
pub fn wrap_footer_message(text: &str, width: u16, reserve: u16) -> Vec<String> {
    let max = usize::from(width.max(12))
        .saturating_sub(usize::from(reserve))
        .max(6);
    let fits = |s: &str, extra: usize| Span::raw(s).width() + extra <= max;
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::from(" ");
    for word in text.split_whitespace() {
        let sep = usize::from(cur.len() > 1);
        if !fits(&cur, sep + Span::raw(word).width()) && cur.len() > 1 {
            lines.push(std::mem::replace(&mut cur, String::from(" ")));
        }
        if cur.len() > 1 {
            cur.push(' ');
        }
        if fits(&cur, Span::raw(word).width()) {
            cur.push_str(word);
        } else {
            for c in word.chars() {
                if !fits(&cur, 2) {
                    lines.push(std::mem::replace(&mut cur, String::from(" ")));
                }
                cur.push(c);
            }
        }
    }
    if cur.len() > 1 || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Keep the TAIL of a typed input so the cursor end stays visible when the
/// text outgrows the prompt line (shell-style).
pub fn input_tail(input: &str, max: usize) -> String {
    if Span::raw(input).width() <= max {
        return input.to_string();
    }
    let mut out = String::new();
    for c in input.chars().rev() {
        let mut candidate = c.to_string();
        candidate.push_str(&out);
        if Span::raw(candidate.as_str()).width() + 1 > max {
            break;
        }
        out = candidate;
    }
    format!("…{out}")
}

pub fn truncate_to(s: String, max: usize) -> String {
    if Span::raw(s.as_str()).width() <= max {
        return s;
    }
    if max < 2 {
        return String::new();
    }
    let mut out = String::new();
    for c in s.chars() {
        let mut candidate = out.clone();
        candidate.push(c);
        if Span::raw(candidate.as_str()).width() + 1 > max {
            break;
        }
        out = candidate;
    }
    out.push('…');
    out
}

/// Widest a one-line input dialog gets in a sidebar pane. Kept below a wide
/// pane's width so the box reads as a dialog, not a footer strip.
pub const INPUT_DIALOG_MAX_WIDTH: u16 = 60;

/// Rect of a one-line input dialog: fitted to the pane, never wider than
/// `max` cells, centered, one third of the way down. Also never narrower than
/// the label on the border needs (still capped by `max` and by `area`), so a
/// narrow pane can show what it is asking for.
pub fn input_dialog_rect(area: Rect, title: &str, max: u16) -> Rect {
    dialog_rect(area, dialog_width(area, title, max), 3)
}

/// Width shared by every dialog: fitted to the pane, capped at `max`, and wide
/// enough for the border label (see `input_dialog_rect`).
fn dialog_width(area: Rect, title: &str, max: u16) -> u16 {
    let max = max.max(1);
    let label_width = u16::try_from(Span::raw(title).width() + 4).unwrap_or(u16::MAX);
    area.width
        .saturating_sub(2)
        .clamp(1, max)
        .max(label_width.min(max))
        .min(area.width)
}

/// A `width` by `height` dialog (height capped by `area`), centered
/// horizontally and one third of the way down.
fn dialog_rect(area: Rect, width: u16, height: u16) -> Rect {
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    )
}

/// Message rows a confirm dialog shows in a box `inner_height` rows tall: the
/// last row is reserved for the buttons, so a message cut short by a small pane
/// never pushes the buttons off the box.
fn confirm_body_lines(message: &str, inner_width: u16, inner_height: u16) -> Vec<String> {
    let mut lines = wrap_footer_message(message, inner_width, 0);
    lines.truncate(usize::from(inner_height.saturating_sub(1)));
    lines
}

/// Rect of a y/N confirm dialog: the input dialog's width rule, and a height of
/// the border rows, the wrapped message and one button row (capped by `area`).
pub fn confirm_dialog_rect(area: Rect, title: &str, message: &str) -> Rect {
    let width = dialog_width(area, title, INPUT_DIALOG_MAX_WIDTH);
    let inner_width = width.saturating_sub(2);
    let body = wrap_footer_message(message, inner_width, 0).len();
    let height = u16::try_from(body + 3).unwrap_or(u16::MAX);
    dialog_rect(area, width, height)
}

/// The confirm buttons' layout for `dialog`: each button's rect and label, Yes
/// first. Both `confirm_button_rects` and the drawing read it, so the clicks
/// always land on what is drawn. The widest form is padded labels with one
/// cell before the right border; a narrow box drops the padding, then the
/// border gap. `None` when even `Yes No` does not fit, or the box has no
/// button row.
fn confirm_buttons(dialog: Rect) -> Option<[(Rect, &'static str); 2]> {
    // (yes label, no label, cells kept clear before the right border)
    const FORMS: [(&str, &str, u16); 3] =
        [(" Yes ", " No ", 1), ("Yes", "No", 1), ("Yes", "No", 0)];
    let inner_width = dialog.width.saturating_sub(2);
    if dialog.height < 3 || inner_width == 0 {
        return None;
    }
    let row = dialog.y + dialog.height - 2;
    // One cell between the two buttons.
    let (yes, no, margin) = FORMS.into_iter().find(|(yes, no, margin)| {
        let fits = u16::try_from(yes.len() + no.len() + 1).unwrap_or(u16::MAX);
        fits.saturating_add(*margin) <= inner_width
    })?;
    let yes_w = u16::try_from(yes.len()).unwrap_or(u16::MAX);
    let no_w = u16::try_from(no.len()).unwrap_or(u16::MAX);
    let right = dialog.x + 1 + inner_width;
    let no_x = right - margin - no_w;
    let yes_x = no_x - 1 - yes_w;
    Some([
        (Rect::new(yes_x, row, yes_w, 1), yes),
        (Rect::new(no_x, row, no_w, 1), no),
    ])
}

/// The `(yes, no)` button rects of a confirm dialog box `dialog` (the rect from
/// `confirm_dialog_rect`): 1-row rects on its last inner row, Yes left of No.
/// Both are zero-sized when the box cannot show them, so no click can hit an
/// invisible button; the keyboard still answers.
pub fn confirm_button_rects(dialog: Rect) -> (Rect, Rect) {
    match confirm_buttons(dialog) {
        Some([(yes, _), (no, _)]) => (yes, no),
        None => (Rect::default(), Rect::default()),
    }
}

/// Draw a y/N confirm (`title` on the border, the wrapped `message`, and a row
/// of `Yes` / `No` buttons on the last row) as a box over the body. The footer
/// used to carry these questions in red; a box keeps them in view. `hover` is
/// the mouse cell, which highlights the button under it. Returns the `(yes, no)`
/// button rects for click handling.
pub fn draw_confirm_dialog(
    frame: &mut Frame,
    title: &str,
    message: &str,
    hover: Option<(u16, u16)>,
) -> (Rect, Rect) {
    let rect = confirm_dialog_rect(frame.area(), title, message);
    // A block TITLE is silently clipped rather than wrapped, so trim it to the
    // border's usable width first.
    let label = truncate_to(title.to_string(), usize::from(rect.width.saturating_sub(4)));
    let inner_width = rect.width.saturating_sub(2);
    let inner_height = rect.height.saturating_sub(2);

    let lines: Vec<Line> = confirm_body_lines(message, inner_width, inner_height)
        .into_iter()
        .map(Line::from)
        .collect();

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(format!(" {label} "))
                .border_style(Style::default().fg(palette().accent)),
        ),
        rect,
    );

    let colors = palette();
    if let Some(buttons) = confirm_buttons(rect) {
        for (button, text) in buttons {
            let hovered = hover.is_some_and(|(x, y)| hits(button, x, y));
            let style = if hovered {
                Style::default()
                    .bg(colors.button_focus_bg)
                    .fg(colors.button_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(colors.button_bg).fg(colors.button_fg)
            };
            frame.render_widget(Paragraph::new(Line::styled(text, style)), button);
        }
    }
    confirm_button_rects(rect)
}

/// Draw a one-line input dialog (`title` on the border, `input`'s TAIL so the
/// caret end stays visible, ⏎ ok · esc cancel while it fits) as a box over the
/// body instead of burying the prompt in the footer. Returns the box's rect.
pub fn draw_input_dialog(frame: &mut Frame, title: &str, input: &str) -> Rect {
    let rect = input_dialog_rect(frame.area(), title, INPUT_DIALOG_MAX_WIDTH);
    // A block TITLE is silently clipped rather than wrapped, so trim it to the
    // border's usable width first.
    let label = truncate_to(title.to_string(), usize::from(rect.width.saturating_sub(4)));

    let inner_width = usize::from(rect.width.saturating_sub(2));
    // Drop the hint before the typed text loses room.
    let hint = "  ⏎ ok · esc cancel";
    let typed = input_tail(input, inner_width.saturating_sub(3));
    let hint_fits = Span::raw(typed.as_str()).width() + Span::raw(hint).width() + 2 <= inner_width;

    let mut spans = vec![
        Span::raw(" "),
        // Tail, not head: a long name must keep its end — where the cursor
        // is — on screen.
        Span::raw(typed),
        Span::styled("█", Style::default().dim()),
    ];
    if hint_fits {
        spans.push(Span::styled(hint, Style::default().dim()));
    }

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::bordered()
                .title(format!(" {label} "))
                .border_style(Style::default().fg(palette().accent)),
        ),
        rect,
    );
    rect
}

/// Pane ids in the same tab as `my_pane_id` that belong to the `other` view
/// (matched by its standalone label or its metadata token), from a
/// `pane.list` response.
pub fn sibling_panes_of(pane_list_json: &str, my_pane_id: &str, other: View) -> Vec<String> {
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
        label: Option<String>,
        tab_id: Option<String>,
        #[serde(default)]
        tokens: serde_json::Map<String, serde_json::Value>,
    }
    let Ok(msg) = serde_json::from_str::<Msg>(pane_list_json.trim_start_matches('\u{feff}')) else {
        return Vec::new();
    };
    let panes = &msg.result.panes;
    let Some(my_tab) = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(my_pane_id))
        .and_then(|p| p.tab_id.clone())
    else {
        return Vec::new();
    };
    panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(my_tab.as_str()))
        .filter(|p| p.pane_id.as_deref() != Some(my_pane_id))
        .filter(|p| {
            p.label.as_deref() == Some(other.label()) || p.tokens.contains_key(other.plugin_id())
        })
        .filter_map(|p| p.pane_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encloses(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.width <= outer.x + outer.width
            && inner.y + inner.height <= outer.y + outer.height
    }

    #[test]
    fn input_dialog_centers_and_stays_inside_a_normal_pane() {
        let area = Rect::new(0, 0, 32, 20);
        let rect = input_dialog_rect(area, "Rename", INPUT_DIALOG_MAX_WIDTH);
        assert!(encloses(area, rect), "{rect:?} escaped {area:?}");
        // One cell of inset each side, one bordered content row.
        assert_eq!((rect.width, rect.height), (30, 3));
        assert_eq!(rect.x, 1);
        // A third of the way down, never the very first row.
        assert_eq!(rect.y, (20 - 3) / 3);
        // A wide pane caps the box at `max` and still centers it.
        let wide = Rect::new(10, 0, 200, 40);
        let capped = input_dialog_rect(wide, "Custom editor", INPUT_DIALOG_MAX_WIDTH);
        assert_eq!(capped.width, INPUT_DIALOG_MAX_WIDTH);
        assert_eq!(capped.x, 10 + (200 - INPUT_DIALOG_MAX_WIDTH) / 2);
        assert!(encloses(wide, capped));
    }

    #[test]
    fn input_dialog_clamps_into_tiny_panes() {
        for area in [
            Rect::new(0, 0, 6, 2),
            Rect::new(4, 7, 0, 0),
            Rect::new(2, 3, 1, 1),
            Rect::new(0, 0, 3, 0),
        ] {
            let rect = input_dialog_rect(area, "Custom editor", INPUT_DIALOG_MAX_WIDTH);
            assert!(encloses(area, rect), "{rect:?} escaped {area:?}");
            assert!(rect.width <= INPUT_DIALOG_MAX_WIDTH);
        }
    }

    #[test]
    fn confirm_dialog_fits_narrow_and_wide_panes() {
        let message = "Delete 'a-rather-long-file-name.rs' permanently?";
        for area in [Rect::new(0, 0, 30, 20), Rect::new(0, 0, 200, 50)] {
            let rect = confirm_dialog_rect(area, "Delete file", message);
            assert!(encloses(area, rect), "{rect:?} escaped {area:?}");
            assert!(rect.width <= INPUT_DIALOG_MAX_WIDTH);
            assert_eq!(rect.y, (area.height - rect.height) / 3, "one third down");
        }
        // The narrow box keeps the one-cell gutter and centers.
        let narrow = confirm_dialog_rect(Rect::new(0, 0, 30, 20), "Delete file", message);
        assert_eq!(narrow.width, 28);
        assert_eq!(narrow.x, 1);
        // A wide pane caps the width at the input dialog maximum.
        let wide = confirm_dialog_rect(Rect::new(0, 0, 200, 50), "Delete file", message);
        assert_eq!(wide.width, INPUT_DIALOG_MAX_WIDTH);
        assert_eq!(wide.x, (200 - INPUT_DIALOG_MAX_WIDTH) / 2);
    }

    #[test]
    fn confirm_dialog_grows_with_the_message_and_stays_in_the_pane() {
        let area = Rect::new(0, 0, 30, 40);
        let short = confirm_dialog_rect(area, "Discard changes", "Discard?");
        // Borders + one message row + hint row.
        assert_eq!(short.height, 4);
        let long_message = "Reset current branch to origin/main with a mixed reset and keep the \
                            working tree as it is, then carry on as before for ever and ever.";
        let long = confirm_dialog_rect(area, "Discard changes", long_message);
        assert!(long.height > short.height, "{long:?} vs {short:?}");
        assert!(encloses(area, long));
        // A tiny pane clamps the height and keeps the box inside.
        let tiny = Rect::new(2, 3, 12, 3);
        let clipped = confirm_dialog_rect(tiny, "Discard changes", long_message);
        assert!(encloses(tiny, clipped), "{clipped:?} escaped {tiny:?}");
        assert!(clipped.height <= tiny.height);
    }

    #[test]
    fn confirm_body_keeps_the_button_row() {
        // A message longer than the box loses its tail, never the button row.
        let lines = confirm_body_lines(&"word ".repeat(40), 10, 3);
        assert_eq!(lines.len(), 2, "inner height 3 minus the button row");
    }

    #[test]
    fn confirm_buttons_sit_side_by_side_on_the_last_row() {
        let message = "Delete 'a-rather-long-file-name.rs' permanently?";
        for area in [Rect::new(0, 0, 30, 20), Rect::new(0, 0, 200, 50)] {
            let dialog = confirm_dialog_rect(area, "Delete file", message);
            let (yes, no) = confirm_button_rects(dialog);
            let last_row = dialog.y + dialog.height - 2;
            for button in [yes, no] {
                assert_eq!(button.y, last_row, "{button:?} off the last inner row");
                assert_eq!(button.height, 1);
                assert!(encloses(dialog, button), "{button:?} escaped {dialog:?}");
                // Never on the border columns.
                assert!(button.x > dialog.x && button.x + button.width < dialog.x + dialog.width);
            }
            assert!(
                yes.x + yes.width < no.x,
                "buttons overlap or touch: {yes:?} {no:?}"
            );
            // Padded labels keep one cell before the right border.
            assert_eq!(no.x + no.width, dialog.x + dialog.width - 2);
        }
    }

    #[test]
    fn confirm_buttons_narrow_fallbacks_stay_inside_the_box() {
        // Inner width 10: padded labels (11 cells) no longer fit, bare ones do.
        let dialog = Rect::new(0, 0, 12, 5);
        let (yes, no) = confirm_button_rects(dialog);
        assert_eq!((yes.width, no.width), (3, 2), "bare labels");
        assert!(yes.x < no.x && encloses(dialog, yes) && encloses(dialog, no));
        // Inner width 6: no border gap is left, the bare pair sits flush.
        let flush = Rect::new(0, 0, 8, 5);
        let (yes, no) = confirm_button_rects(flush);
        assert_eq!(no.x + no.width, flush.x + flush.width - 1);
        assert!(yes.x + yes.width < no.x && encloses(flush, no));
        // Inner width 5: even `YesNo` does not fit, so nothing is drawn or hit.
        let (yes, no) = confirm_button_rects(Rect::new(0, 0, 7, 5));
        assert_eq!((yes, no), (Rect::default(), Rect::default()));
        // No button row at all in a box two rows tall.
        let (yes, no) = confirm_button_rects(Rect::new(0, 0, 40, 2));
        assert_eq!((yes, no), (Rect::default(), Rect::default()));
    }

    /// Perceived brightness on a 0..1 scale, the measure `darken_for_light`
    /// caps.
    fn luma(color: Color) -> f32 {
        match color {
            Color::Rgb(r, g, b) => {
                (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
            }
            other => panic!("expected an RGB color, got {other:?}"),
        }
    }

    #[test]
    fn light_palette_foregrounds_stay_dark_and_backgrounds_stay_light() {
        let light = palette_for(ColorTheme::Light);
        // Every status/text color must be readable ON WHITE …
        for fg in [
            light.modified,
            light.untracked,
            light.added,
            light.renamed,
            light.deleted,
            light.conflict,
            light.keycap_fg,
            light.header_accent,
            light.accent,
            light.muted_button_fg,
            light.button_fg,
            light.sync_fg,
            light.warning,
            light.diff_del_mark,
            light.diff_add_mark,
        ] {
            assert!(luma(fg) < 0.55, "foreground {fg:?} is too pale on white");
        }
        // … and every row/tint background must stay a light wash, so the
        // terminal's dark default foreground still reads on top of it.
        for bg in [
            light.selection_bg,
            light.selection_unfocused_bg,
            light.activity_hover_bg,
            light.hover_bg,
            light.keycap_bg,
            light.text_selection_bg,
            light.diff_del_bg,
            light.diff_add_bg,
            light.diff_del_word_bg,
            light.diff_add_word_bg,
            light.sync_bg,
            light.muted_button_bg,
            light.button_bg,
            light.button_focus_bg,
        ] {
            assert!(luma(bg) > 0.6, "background {bg:?} is too dark for white");
        }
        // ✓ Commit is a soft tint with dark blue text here, so the pair must
        // hold together on its own — and the label may never be a NAMED color:
        // `Color::White` is ANSI 15, which a light profile draws as pale grey.
        assert!(luma(light.button_bg) > 0.6 && luma(light.button_focus_bg) > 0.6);
        assert!(luma(light.activity_hover_bg) > luma(light.selection_bg));
        assert!(luma(light.button_fg) < 0.35);
        assert!(
            luma(light.muted_button_bg) > luma(light.button_bg),
            "the inactive repo's button must read weaker than the active one"
        );
    }

    #[test]
    fn light_selection_names_its_own_foreground() {
        // The dark themes let the terminal's foreground show through the
        // selection; on a light background that leaves dark-on-dark, so the
        // light palette must state a foreground of its own.
        let focused = selection_style_for(ColorTheme::Light, true);
        assert_eq!(focused.bg, Some(LIGHT_PALETTE.selection_bg));
        assert_eq!(focused.fg, Some(LIGHT_PALETTE.selection_fg));
        assert!(focused.add_modifier.contains(Modifier::BOLD));
        let unfocused = selection_style_for(ColorTheme::Light, false);
        assert_eq!(unfocused.fg, Some(LIGHT_PALETTE.selection_unfocused_fg));
        // `Color::Reset` is "leave it alone", never an emitted foreground.
        assert_eq!(selection_style_for(ColorTheme::VsCode, true).fg, None);
        assert_eq!(hover_style_for(ColorTheme::Light).fg, None);
    }

    #[test]
    fn activity_buttons_select_hover_and_idle_in_that_order() {
        let active = activity_button_style(true, true);
        assert!(active.add_modifier.contains(Modifier::BOLD));
        assert!(!active.add_modifier.contains(Modifier::DIM));

        let hovered = activity_button_style(false, true);
        assert_eq!(hovered.bg, Some(palette().activity_hover_bg));
        assert!(!hovered.add_modifier.contains(Modifier::DIM));

        let idle = activity_button_style(false, false);
        assert!(idle.add_modifier.contains(Modifier::DIM));
        assert!(idle.bg.is_none());
    }

    #[test]
    fn activity_button_hitbox_matches_all_three_highlight_rows() {
        for row in 4..=6 {
            assert!(hits_activity_button((10, 14), 5, 12, row));
        }
        assert!(!hits_activity_button((10, 14), 5, 9, 5));
        assert!(!hits_activity_button((10, 14), 5, 12, 7));
    }

    #[test]
    fn icon_colors_are_dimmed_only_for_the_light_theme() {
        // The JS yellow from `icons::material` — unreadable on white as-is.
        let js = (0xf1, 0xe0, 0x5a);
        let dimmed = darken_for_light(js);
        assert!(luma(Color::Rgb(dimmed.0, dimmed.1, dimmed.2)) <= 0.43);
        // Hue survives: the channel order is unchanged.
        assert!(dimmed.0 > dimmed.2 && dimmed.1 > dimmed.2);
        // A color that is already dark enough passes through untouched.
        let ruby = (0x70, 0x15, 0x16);
        assert_eq!(darken_for_light(ruby), ruby);
        // `set_color_theme` is process-global and tests run in parallel —
        // drive the theme-taking form instead of mutating it here.
        assert_eq!(
            icon_style_for(ColorTheme::VsCode, Some(js)).fg,
            Some(Color::Rgb(js.0, js.1, js.2))
        );
        assert_eq!(
            icon_style_for(ColorTheme::Light, Some(js)).fg,
            Some(Color::Rgb(dimmed.0, dimmed.1, dimmed.2))
        );
        assert_eq!(icon_style_for(ColorTheme::Light, None).fg, None);
    }

    #[test]
    fn terminal_palette_uses_terminal_mapped_ansi_colors() {
        let terminal = palette_for(ColorTheme::Terminal);
        let vscode = palette_for(ColorTheme::VsCode);
        assert_eq!(terminal.modified, Color::Yellow);
        assert_eq!(terminal.untracked, Color::Green);
        assert_eq!(terminal.accent, Color::Blue);
        assert_eq!(terminal.selection_bg, Color::DarkGray);
        assert_eq!(vscode.modified, Color::Rgb(0xe2, 0xc0, 0x8d));
        assert_eq!(vscode.accent, Color::Rgb(0x00, 0x78, 0xd4));
        assert_eq!(
            selection_style_for(ColorTheme::Terminal, true).bg,
            Some(Color::DarkGray)
        );
        assert_eq!(
            selection_style_for(ColorTheme::Terminal, true).fg,
            Some(Color::White)
        );
        assert!(
            !selection_style_for(ColorTheme::Terminal, true)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        let hover = hover_style_for(ColorTheme::Terminal);
        assert_eq!(hover.bg, Some(Color::Black));
        assert_eq!(hover.fg, Some(Color::Gray));
        assert_eq!(
            selection_style_for(ColorTheme::Terminal, false).fg,
            Some(Color::White)
        );
    }

    #[test]
    fn settings_scroll_keeps_the_selected_row_visible() {
        assert_eq!(keep_visible_scroll(0, 5, 12), 0);
        assert_eq!(keep_visible_scroll(4, 5, 12), 0);
        assert_eq!(keep_visible_scroll(5, 5, 12), 1);
        assert_eq!(keep_visible_scroll(11, 5, 12), 7);
        assert_eq!(keep_visible_scroll(2, 0, 12), 3);
    }

    #[test]
    fn messages_wrap_to_narrow_panes() {
        // The reported clip: a delete confirm in a ~26-col pane.
        let lines = wrap_footer_message("Delete 'test' permanently? (y/N)", 26, 4);
        assert!(lines.len() > 1, "must wrap, not clip: {lines:?}");
        for l in &lines {
            assert!(l.starts_with(' '), "leading space kept: {l:?}");
            assert!(
                ratatui::text::Span::raw(l.as_str()).width() <= 22,
                "line within width-reserve: {l:?}"
            );
        }
        assert_eq!(
            lines
                .join("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "Delete 'test' permanently? (y/N)",
            "no words lost"
        );
        // Wide panes stay single-line.
        assert_eq!(
            wrap_footer_message("Delete 'test' permanently? (y/N)", 80, 4).len(),
            1
        );
        // An unbreakable long word hard-breaks instead of overflowing.
        let long = wrap_footer_message("Deleted averyveryverylongfilename.extension", 20, 4);
        assert!(long.len() > 1);
        // Empty input still yields one (empty) line — footers size from len().
        assert_eq!(wrap_footer_message("", 30, 4).len(), 1);
    }

    #[test]
    fn input_tail_keeps_cursor_end_visible() {
        assert_eq!(input_tail("short", 10), "short");
        let tail = input_tail("a-very-long-typed-filename.txt", 12);
        assert!(tail.starts_with('…'), "{tail:?}");
        assert!(tail.ends_with(".txt"), "{tail:?}");
        assert!(ratatui::text::Span::raw(tail.as_str()).width() <= 12);
    }

    #[test]
    fn hints_wrap_instead_of_clipping() {
        let hints = [("⏎", "stage"), ("a", "all"), ("u", "none"), ("q", "quit")];
        let wide = wrap_hints(&hints, 80, 0);
        assert_eq!(wide.len(), 1);
        let narrow = wrap_hints(&hints, 14, 0);
        assert!(narrow.len() >= 2, "narrow pane stacks hints");
        for line in &narrow {
            assert!(line.width() <= 14, "no line exceeds the pane width");
        }
    }

    #[test]
    fn hints_have_no_line_cap() {
        let hints = [
            ("a", "aaa"),
            ("b", "bbb"),
            ("c", "ccc"),
            ("d", "ddd"),
            ("e", "eee"),
            ("f", "fff"),
            ("g", "ggg"),
            ("h", "hhh"),
        ];
        // Every chip lands on its own line rather than overflowing the
        // width — there is no line cap to clip against anymore.
        let lines = wrap_hints(&hints, 10, 0);
        assert_eq!(lines.len(), hints.len());
        for line in &lines {
            assert!(line.width() <= 10, "no line exceeds the pane width");
        }
    }

    #[test]
    fn truncation_keeps_width_budget() {
        assert_eq!(truncate_to("short".into(), 10), "short");
        let cut = truncate_to("averylongdirectoryname".into(), 8);
        assert!(cut.ends_with('…'));
        assert!(Span::raw(cut.as_str()).width() <= 8);
        assert_eq!(truncate_to("abc".into(), 1), "");
    }

    #[test]
    fn title_action_zones_are_contiguous_and_match_width() {
        for theme in [IconTheme::Material, IconTheme::Emoji] {
            let actions = [
                TitleAction::NewFile,
                TitleAction::NewFolder,
                TitleAction::Refresh,
                TitleAction::CollapseAll,
            ];
            let (spans, zones) = title_action_spans(theme, &actions, 10, 0, None);
            assert_eq!(spans.len(), 4);
            let mut x = 10;
            for (rect, _) in &zones {
                assert_eq!(rect.x, x, "chips tile left to right with no gaps");
                x += rect.width;
            }
            let total: u16 = zones.iter().map(|(r, _)| r.width).sum();
            assert_eq!(total, title_actions_width(theme, &actions));
            // A click inside the second chip maps to New Folder.
            let (rect, action) = zones[1];
            assert!(hits(rect, rect.x, 0));
            assert_eq!(action, TitleAction::NewFolder);
        }
    }

    #[test]
    fn title_actions_hide_without_recent_mouse() {
        assert!(!title_actions_visible(None));
        assert!(title_actions_visible(Some(std::time::Instant::now())));
        let old =
            std::time::Instant::now() - TITLE_ACTIONS_LINGER - std::time::Duration::from_secs(1);
        assert!(!title_actions_visible(Some(old)));
    }

    #[test]
    fn sibling_lookup_matches_label_or_token() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","label":"Sidebar"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","label":"Source Control"},
            {"pane_id":"w1:p3","tab_id":"w1:t1","tokens":{"herdr-sidebar-git":{}}},
            {"pane_id":"w1:p9","tab_id":"w1:t2","label":"Source Control"}
        ]}}"#;
        let found = sibling_panes_of(json, "w1:p1", View::SourceControl);
        assert_eq!(found, ["w1:p2", "w1:p3"], "same tab only, label or token");
    }
}
