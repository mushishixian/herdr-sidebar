# dotfiles/ — instructions for coding agents

**This is the IDE layer, and it is a living doc.** Read this before changing anything under `dotfiles/`,
and before changing any sidebar behaviour that the IDE depends on.

## Purpose and scope

`dotfiles/` holds everything that turns herdr + the patched herdr-sidebar + Fresh into the VS Code-shaped
terminal IDE: herdr and Fresh configs, Fresh plugins, the `dotfiles-ide` herdr plugin (layout, terminal,
preview host, agent title), the zsh and lazygit configs, and the installers (`install.sh`,
`herdr-sidebar/build.sh`). The Rust crate lives in `plugins/herdr-sidebar` and never references this
directory. `docs/herdr-ide.md` is the user-facing manual. `docs/install-herdr-ide.sh` is the one-shot
installer and verifier. Record non-obvious findings (herdr, Fresh or WSL behaviour, install traps) in
`docs/herdr-ide.md` (*Gotchas*), and in the root `AGENTS.md` when they concern the Rust side.

## When you change X, also update Y

| When you change | Also update |
|---|---|
| A herdr key binding (`dotfiles/config/herdr/config.toml`) | The herdr key table in `docs/herdr-ide.md` (*Keys → herdr, global*); the verify list in `docs/install-herdr-ide.sh` if the action is one it should check |
| A herdr action or plugin entry point | The manifest that owns it (`plugins/herdr-sidebar/herdr-plugin.toml` or `dotfiles/herdr-plugins/ide/herdr-plugin.toml`); for a sidebar action with a keybinding, the flag mapping in `announce_flag` in `plugins/herdr-sidebar/src/shortcut_tip.rs` so its toast shows the key; the verify list in `docs/install-herdr-ide.sh` |
| A Fresh key binding (`dotfiles/config/fresh/config.json`) | The Fresh key table in `docs/herdr-ide.md` (*Keys → Fresh*); the `fresh config show` grep list in `docs/install-herdr-ide.sh` |
| A Fresh plugin (`dotfiles/config/fresh/plugins/*.ts`) | The *Fresh plugins* section of `docs/herdr-ide.md`; `docs/install-herdr-ide.sh` verify list if it should be checked; the plugin link loop in `dotfiles/install.sh` picks new `*.ts` files up automatically, but a rerun is still needed |
| A script the user runs by name from PATH (`dotfiles/herdr-plugins/ide/bin/*`) | The `for script in …` link list in `dotfiles/install.sh`. Scripts that only herdr runs (manifest `command = ["./bin/…"]`) need no link |
| An apt, npm or cargo dependency | `dotfiles/packages.txt` (apt) or the installer step (npm/cargo) in `dotfiles/install.sh` or `docs/install-herdr-ide.sh`; the *Install* section of `docs/herdr-ide.md`; a check in the verify step |
| A sidebar setting default | `dotfiles/herdr-sidebar/state.json`, and the *Sidebar defaults* paragraph in `docs/herdr-ide.md` (*herdr-sidebar fork*) |
| Anything the sidebar (Rust) and the dotfiles scripts share (env vars such as `HERDR_SIDEBAR_OPEN_DIFF`, `HERDR_SIDEBAR_PREVIEW_HOSTED`, `FRESH_IDE_DAEMON`, `DOTFILES_IDE_BIN`; pane tokens such as `fresh_daemon`, `hs-preview-host`; request files such as the AI-commit request, the live-diff request and `preview-host/<pane>.req`; `editor-command.txt`) | Both sides in the same change. Document the contract in the root `AGENTS.md` (Rust side) and in `docs/herdr-ide.md` |
| Pane labels `Fresh`, `Terminal`, `Agent`, or the plugin id `dotfiles-ide` | Do not rename them: scripts, state dirs and key bindings look them up. Show new display text through a metadata title (`pane report-metadata --title`) |
| Any user-visible behaviour | `docs/herdr-ide.md`: the feature section, *Keys* and *Troubleshooting*; the fork commit table (*herdr-sidebar fork*): its `*uncommitted*` row until the user commits, then the hash |
| Install, upgrade or migration behaviour | `docs/install-herdr-ide.sh` (and its `--help` text, the `sed -n '2,10p'` header) and `dotfiles/install.sh` (its `--help` header, `sed -n '2,13p'`); the *Install*, *Migrating* and *Updating* sections of `docs/herdr-ide.md` |

## Verify before you finish

- Shell: `bash -n` and `shellcheck` on every touched script (`dotfiles/**/*.sh`, `dotfiles/herdr-plugins/ide/bin/*`,
  `docs/install-herdr-ide.sh`). The zsh hook (`shell/agent-title.zsh`) gets `zsh -n`.
- Rust (only if `plugins/herdr-sidebar` changed): from `plugins/herdr-sidebar`, run
  `cargo build --release && cargo test && cargo clippy -- -D warnings`.
- Installer: run only the read-only verify part when you need it. Do not run the full install, and do not
  run `dotfiles/install.sh` on the user's machine unless they ask.
- Live tests: only in throwaway herdr tabs (`herdr tab create --no-focus …`), closed afterwards.
  Never restart, kill or rename the user's own `ide-*` Fresh daemons. Use a throwaway daemon name for tests.
- After deploying sidebar changes: `herdr plugin action invoke herdr-sidebar.redeploy` (or rebuild with
  `dotfiles/herdr-sidebar/build.sh` first). Running sidebars keep the old binary until redeployed.
- Fresh config or plugin changes need the user's `dotfiles/herdr-plugins/ide/bin/fresh-restart --all`.
  Ask the user to run it; do not run it yourself.

## Gotchas (verified)

- Fresh's settings UI replaces the symlinked `~/.config/fresh/config.json` with a plain copy, and later
  dotfiles edits then do nothing. Merge the copy into `dotfiles/config/fresh/config.json`, then rerun the
  installer to re-link. Edit settings in the dotfiles file, not in the UI.
- `HERDR_PLUGIN_STATE_DIR` names the CALLING plugin's state dir in actions and hooks. A script that reads
  another plugin's state must compute that path itself.
- Popups (`popup` placement) belong to no tab. A process started from a popup has no usable `HERDR_TAB_ID`
  or `HERDR_WORKSPACE_ID`, so pass the caller pane's ids explicitly (`HERDR_PANE_ID`, `HERDR_TAB_ID`,
  `HERDR_WORKSPACE_ID`).
- `pane report-metadata` MERGES tokens. Sending `{}` does nothing, and only an explicit null value removes
  a token. Token values must be strings.
- `Ctrl+Alt+H` arrives as Alt+Backspace in terminals without the kitty protocol, so replace is on Ctrl+Alt+E.
- Windows Terminal swallows Ctrl+Shift+A (select all) and Ctrl+Tab (next tab); the Windows Terminal and
  Pebrel settings must unbind them, or the AI-commit and recent-tab keys never arrive.
- `npm install -g` needs `--prefix ~/.local`: the global prefix is root-owned.
- `herdr` started from an agent shell passes `NO_COLOR=1` to every pane. Start herdr from a normal shell.
- `pane.send_text` sends raw bytes (a newline submits an agent prompt); `pane.send_input` bracket-pastes.

## Commits

Never commit unless the user asks. The user makes the commits. When they do, fill in the `*uncommitted*`
rows in `docs/herdr-ide.md` with the hash.
