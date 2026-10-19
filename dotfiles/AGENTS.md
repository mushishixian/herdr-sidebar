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
| A language server | `dotfiles/install-lsp.sh` (its `servers` table and install step); the `lsp` block in `dotfiles/config/fresh/config.json`; the verify list in `docs/install-herdr-ide.sh`; the *Language servers (LSP)* table in `docs/herdr-ide.md` |
| A sidebar setting default | `dotfiles/herdr-sidebar/state.json`, and the *Sidebar defaults* paragraph in `docs/herdr-ide.md` (*herdr-sidebar fork*) |
| Anything the sidebar (Rust) and the dotfiles scripts share (env vars such as `HERDR_SIDEBAR_OPEN_DIFF`, `HERDR_SIDEBAR_PREVIEW_HOSTED`, `FRESH_IDE_DAEMON`, `DOTFILES_IDE_BIN`; pane tokens such as `fresh_daemon`, `hs-preview-host`; request files such as the AI-commit request, the live-diff request, `preview-host/<pane>.req` and the dock-suppression stamp `<herdr-sidebar state dir>/dock/restore.stamp` that `ide-save-sizes` reads; `editor-command.txt`; the sidebar's `state.json` read at build time for `terminal_height` and `agent_width`) | Both sides in the same change. Document the contract in the root `AGENTS.md` (Rust side) and in `docs/herdr-ide.md` |
| Pane labels `Fresh`, `Terminal`, `Agent`, or the plugin id `dotfiles-ide` | Do not rename them: scripts, state dirs and key bindings look them up. Show new display text through a metadata title (`pane report-metadata --title`) |
| Any user-visible behaviour | `docs/herdr-ide.md`: the feature section, *Keys* and *Troubleshooting*; the fork commit table (*herdr-sidebar fork*): its `*uncommitted*` row until the user commits, then the hash |
| Install, upgrade or migration behaviour | `docs/install-herdr-ide.sh` (and its `--help` text, the `sed -n '2,11p'` header) and `dotfiles/install.sh` (its `--help` header, `sed -n '2,14p'`); the *Install*, *Migrating* and *Updating* sections of `docs/herdr-ide.md` |

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

- **The editor/terminal split ratio is the EDITOR (top) share.** `herdr pane split --ratio`
  gives the ORIGINAL pane the ratio, and both build sites split the Terminal (or the shell
  that becomes it) downward and then swap, so the value they pass ends up sizing the editor.
  A terminal of N rows in a column of H rows is therefore `1 - N/H`; `lib.sh`'s
  `editor_split_ratio` is the one place that converts (saved `sizes.json` ratio → sidebar
  `terminal_height` setting → 0.7). Keep it pure (layout JSON on stdin, no `$HERDR` call) so
  it stays testable on fixtures, and never let the sidebar read `sizes.json` back: the
  dependency is one-way (this layer reads `herdr-sidebar/state.json`).
- **The Agent split ratio is the MIDDLE column's share**, the same axis trick on the other side:
  both `herdr-ide` build sites split the anchor pane rightwards (`--ratio` = the ORIGINAL
  anchor's share, which the swap then hands to the middle column), so an Agent column of N
  columns in a W-wide anchor is `1 - N/W`. `lib.sh`'s `agent_split_ratio` is the one place that
  converts (saved `sizes.json` `agent` ratio → sidebar `agent_width` setting → 0.6), and it is
  pure for the same fixture-testing reason.

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
- Fresh starts language servers only in a Trusted workspace (`~/.local/share/fresh/workspaces/<path>/trust.json`).
  A throwaway test folder is Restricted (`LSP (off)`): write a `{"level": "trusted"}` trust file for the
  test folder only, and delete it afterwards.
- rust-analyzer releases since 2026-07-27 refuse toolchains older than 1.94; apt's `rustc` is 1.93, so
  `install-lsp.sh` pins release `2026-07-20` for an older `rustc`. Recheck when apt's Rust moves.
- Under `set -o pipefail`, `cmd | grep -q` can fail although it matched (grep exits early, `cmd` gets
  SIGPIPE). Use `grep … >/dev/null` in conditions.
- `pane.send_text` sends raw bytes (a newline submits an agent prompt); `pane.send_input` bracket-pastes.
- **`ide-save-sizes` must not sample while the sidebar is re-docking.** The sidebar's
  full-height repair rebuilds the tab's splits through a temporary tab and herdr re-creates each
  at a DEFAULT ratio, so a `pane.focused`/`tab.focused` sample taken meanwhile would record that
  intermediate ratio as the user's own. `lib.sh`'s `save_sizes` therefore returns early while
  `recent_dock_reorder` sees a `dock/restore.stamp` (unix seconds, written by the sidebar just
  before it splits) younger than `DOCK_SUPPRESS_SECS` (20 s). The sidebar restores the ratios
  itself; this guard only stops the shell layer from persisting the intermediate state. A
  missing or garbled stamp never suppresses a save.

- **jq ≥ 1.6 reserves `label` as a keyword, so `$label` is a PARSE ERROR** ("unexpected
  label") — Ubuntu/WSL ship jq 1.6. Every pane lookup in `lib.sh`, `ide-save-sizes` and
  `fresh-to-agent` used `--arg label … $label`, so on such a host the whole IDE layer failed:
  clicking a file in the sidebar showed "opened in custom editor: <file>" and then nothing
  happened, because `fresh-open` saw no Fresh pane and `herdr-ide` could not start one. It is
  invisible by design — the sidebar spawns the editor with NULL stdio and only reports the
  SPAWN, `$( … )` swallows the jq exit status, and a hook that ran a broken jq still logs
  "succeeded". The variables are `$lbl`/`$tab`/`$ws`/… now; never name a jq variable
  `label`, `break`, `def`, `as`, `if`, `then`, `else`, `end`, `reduce`, `foreach`, `try`,
  `catch`, `import`, `include`, `and`, `or` or `not`.
- **A herdr server restart leaves every IDE pane a CORPSE with a label.** Resumed panes are
  plain shells: `Fresh`/`Terminal`/`Agent` come back, the metadata tokens (`fresh_daemon`,
  `hs-preview-host`) do not, and no daemon runs. So never treat the label as proof that an
  editor is up — `lib.sh`'s `fresh_editor_ok` (token AND live daemon) is the check. `herdr-ide`
  heals a tokenless `Fresh` pane in place (relabel + `fresh-ide` in the SAME pane; splitting a
  new one would leave the corpse behind and double the middle column) and prints the editor pane
  id on every path, because `fresh-open` reads that id as its last line. Symptom when this
  regressed: the first file click after a restart reported `custom editor failed:
  …/fresh-open exited with exit status: 1` and the `Fresh` pane showed an empty prompt.
  `fresh-restart` deliberately still refuses tokenless panes.
- **`~/.config/herdr/config.toml` must stay the installer's symlink, or the rail and the key
  table silently stop applying.** `install.sh` links it to `dotfiles/config/herdr/config.toml`;
  a user (or an editor) that replaces it with their own file leaves herdr on its defaults —
  `status_indicators = "dots"` (working/blocked/done all one `●`, colour only, which reads as
  "the indicator never changes") and the two-row `[ui.sidebar.spaces]` default with no
  `state_text` word line. Nothing errors: `herdr workspace list` still reports
  `agent_status = working`. Symptom to recognise: "左上角不显示 agent 在 working". Merge the
  `[ui]` + `[ui.sidebar.spaces]` block into the personal file, then
  `herdr server reload-config` (applies to a LIVE client).
- **The Agent column does not assume `claude`.** `lib.sh`'s `agent_command()` resolves the
  command in this order: the plugin's `agent` file (`$(herdr plugin config-dir dotfiles-ide)/agent`,
  the user's explicit choice) → `$IDE_AGENT` → the first agent that is actually INSTALLED,
  **pi before claude** — typing a missing binary into a pane leaves a dead shell that reads as
  "the IDE is broken" (user-reported on a fresh machine). Neither installed → `none`, so no
  Agent column. Get the config dir from the CLI, not a hardcoded `~/.config/herdr`: a user
  with `XDG_CONFIG_HOME` set has a different path.

## Commits

Never commit unless the user asks. The user makes the commits. When they do, fill in the `*uncommitted*`
rows in `docs/herdr-ide.md` with the hash.
