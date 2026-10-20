//! Windowless sidebar sidecar. GUI subsystem on Windows: it must NEVER own a
//! console — a console process launched from a herdr focus hook flashes a
//! Windows Terminal window on Windows 11 even under CREATE_NO_WINDOW, and this
//! runs on every tab/workspace focus. All herdr interaction is socket I/O.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let flag = std::env::args().nth(1).unwrap_or_default();
    herdr_sidebar::shortcut_tip::announce_flag(&flag, std::env::args().nth(2).as_deref());
    match std::env::args().nth(1).as_deref() {
        Some("--update-latest") => {
            if let Err(error) = herdr_sidebar::updates::run() {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
        Some("--refresh-sidebars") => {
            if let Err(error) = herdr_sidebar::updates::refresh() {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
        Some("--git-action") => {
            let op = std::env::args().nth(2).unwrap_or_default();
            if let Err(error) = herdr_sidebar::git_action::run(&op) {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
        Some("--search-popup-open") => {
            // Windows twin of the Unix action: the popup request itself is
            // GUI-subsystem safe (no console is spawned).
            let _ = herdr_sidebar::search_popup::open();
            return;
        }
        Some("--symbol-popup-open") => {
            // Windows twin of the Unix action, same as --search-popup-open.
            let _ = herdr_sidebar::symbol_popup::open();
            return;
        }
        Some("--replace-popup-open") => {
            // Windows twin of the Unix action, same as --search-popup-open.
            let _ = herdr_sidebar::search_popup::open_replace();
            return;
        }
        Some("--git-pull") => {
            if let Err(error) = herdr_sidebar::git_action::run("pull") {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
        Some("--git-push") => {
            if let Err(error) = herdr_sidebar::git_action::run("push") {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
        Some("--git-commit") => {
            if let Err(error) = herdr_sidebar::git_action::run("commit") {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }
    let mode = match std::env::args().nth(1).as_deref() {
        Some("--toggle") => {
            herdr_sidebar::ensure::Mode::Toggle(herdr_sidebar::state::View::Explorer)
        }
        Some("--toggle-git") => {
            herdr_sidebar::ensure::Mode::Toggle(herdr_sidebar::state::View::SourceControl)
        }
        Some("--show-explorer") => {
            herdr_sidebar::ensure::Mode::Activate(herdr_sidebar::ensure::Target::Explorer)
        }
        Some("--show-search") => {
            herdr_sidebar::ensure::Mode::Activate(herdr_sidebar::ensure::Target::Search)
        }
        Some("--show-git") => {
            herdr_sidebar::ensure::Mode::Activate(herdr_sidebar::ensure::Target::SourceControl)
        }
        Some("--quick-open") => {
            herdr_sidebar::ensure::Mode::Activate(herdr_sidebar::ensure::Target::QuickOpen)
        }
        _ => herdr_sidebar::ensure::Mode::Ensure,
    };
    // Errors are deliberately silent: there is no console to print to, herdr
    // logs the exit, and the next focus event retries anyway.
    let _ = herdr_sidebar::ensure::run(mode);
}
