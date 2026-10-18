//! Shared library behind the sidebar binaries: the sidebar TUI
//! (`herdr-sidebar`, hosting BOTH views — file explorer and source
//! control — plus the `--preview` file viewer) and the windowless ensure
//! sidecar (`herdr-sidebar-ensure`).

pub mod actions;
pub mod agent_tokens;
pub mod ai_commit;
pub mod ansi;
pub mod branch_ui;
pub mod content_search;
pub mod diffview;
pub mod editor;
pub mod ensure;
pub mod file_popup;
pub mod file_search;
pub mod fontsetup;
pub mod git;
pub mod git_action;
pub mod git_popup;
pub mod gitdeco;
pub mod icons;
pub mod ipc;
pub mod launch;
pub mod pane_menu;
pub mod rundir;
pub mod search_popup;
pub mod shortcut_tip;
pub mod snooze;
pub mod state;
pub mod suggest;
pub mod syntax;
pub mod takeover;
pub mod tree;
pub mod ui;
pub mod updates;
pub mod viewer;
pub mod wrap;
