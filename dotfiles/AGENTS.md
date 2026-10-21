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
| A herdr key binding (`dotfiles/config/herdr/config.toml`) | The herdr key table in `docs/herdr-ide.md` (*Keys → herdr, global*); a check `dotfiles/verify.sh` can make (it asserts the keymap's script references and `herdr config check`). This file AND `config/herdr/scripts/` are installer symlinks into `~/.config/herdr/`: never edit the `~/.config` copies, a pull cannot reach them |
| A script under `config/herdr/scripts/` (a keymap popup runs `bash ~/.config/herdr/scripts/<name>.sh`) | Keep it plain bash (`bash -n`) with `~`/`$HOME` paths, never a hardcoded `/home/<user>`, or another machine's copy breaks. Record or drop its binding in the key table of `docs/herdr-ide.md`. Add scripts to this directory, not to `~/.config`: the installer symlinks the whole directory |
| A herdr action or plugin entry point | The manifest that owns it (`plugins/herdr-sidebar/herdr-plugin.toml` or `dotfiles/herdr-plugins/ide/herdr-plugin.toml`); for a sidebar action with a keybinding, the flag mapping in `announce_flag` in `plugins/herdr-sidebar/src/shortcut_tip.rs` so its toast shows the key; a `plan PLUGIN`/`plan TOOL` line so `verify.sh` asserts it |
| A Fresh key binding (`dotfiles/config/fresh/config.json`) | The Fresh key table in `docs/herdr-ide.md` (*Keys → Fresh*); the `fresh config show` grep list in `docs/install-herdr-ide.sh` |
| A Fresh plugin (`dotfiles/config/fresh/plugins/*.ts`) | The *Fresh plugins* section of `docs/herdr-ide.md`; a `plan` line in `dotfiles/install.sh` if it should be checked (`verify.sh` then asserts it); the plugin link loop in `dotfiles/install.sh` picks new `*.ts` files up automatically, but a rerun is still needed |
| `~/.zshrc` (managed whole-file, like every other config) | `dotfiles/zsh/zshrc`; the *Install* note and the `~/.zshrc.local` gotcha in `docs/herdr-ide.md`. Keep the `~/.zshrc.local` source line above the plugin sources |
| A script the user runs by name from PATH (`dotfiles/herdr-plugins/ide/bin/*`) | The `for script in …` link list in `dotfiles/install.sh`. Scripts that only herdr runs (manifest `command = ["./bin/…"]`) need no link |
| A Python tool (or a harlequin adapter) | `dotfiles/packages.lock` (`uv` / `uv-with` lines) — never a version in a script; the *Install* note and the adapter gotcha in `docs/herdr-ide.md` |
| An apt, npm or cargo dependency | `dotfiles/packages.txt` (apt) or the installer step (npm/cargo) in `dotfiles/install.sh` or `docs/install-herdr-ide.sh`; the *Install* section of `docs/herdr-ide.md`; a `plan TOOL` line in `install.sh`, which `verify.sh` asserts |
| A language server | `dotfiles/install-lsp.sh` (its `servers` table and install step); the `lsp` block in `dotfiles/config/fresh/config.json`; a `plan TOOL` line in `install.sh`; the *Language servers (LSP)* table in `docs/herdr-ide.md` |
| A sidebar setting default | `dotfiles/herdr-sidebar/state.json`, and the *Sidebar defaults* paragraph in `docs/herdr-ide.md` (*herdr-sidebar fork*) |
| Anything the sidebar (Rust) and the dotfiles scripts share (env vars such as `HERDR_SIDEBAR_OPEN_DIFF`, `HERDR_SIDEBAR_PREVIEW_HOSTED`, `FRESH_IDE_DAEMON`, `DOTFILES_IDE_BIN`; pane tokens such as `fresh_daemon`, `hs-preview-host`; request files such as the AI-commit request, the live-diff request, `preview-host/<pane>.req` and the dock-suppression stamp `<herdr-sidebar state dir>/dock/restore.stamp` that `ide-save-sizes` reads; `editor-command.txt`; the sidebar's `state.json` read at build time for `terminal_height` and `agent_width`) | Both sides in the same change. Document the contract in the root `AGENTS.md` (Rust side) and in `docs/herdr-ide.md` |
| The CodeBuddy hook (`config/codebuddy/hooks/herdr-agent-state.sh`, the `codebuddy_hooks` merge in `install.sh`, the hook events it installs) | The *Which space's agent is busy* section and the Troubleshooting row of `docs/herdr-ide.md`; `verify.sh`'s codebuddy block; the `codebuddy` bullet in the repo `AGENTS.md`. Keep the merge limited to entries naming that script: everything else in `~/.codebuddy/settings.json` belongs to the user |
| Pane labels `Fresh`, `Terminal`, `Agent`, or the plugin id `dotfiles-ide` | Do not rename them: scripts, state dirs and key bindings look them up. Show new display text through a metadata title (`pane report-metadata --title`) |
| Any user-visible behaviour | `docs/herdr-ide.md`: the feature section, *Keys* and *Troubleshooting*; the fork commit table (*herdr-sidebar fork*): its `*uncommitted*` row until the user commits, then the hash |
| Install, upgrade or migration behaviour | `docs/install-herdr-ide.sh` (and its `--help` text, the `sed -n '2,14p'` header) and `dotfiles/install.sh` (its `--help` header, `sed -n '2,16p'`); the *Install*, *Migrating* and *Updating* sections of `docs/herdr-ide.md` |

## Installer contract (dry run, plan, verify)

`dotfiles/install.sh` is the ONE installer, and it is **declarative about what it manages**:

- `--dry-run` changes nothing (no link, no package, no plugin registration, no server reload).
- `dotfiles/packages.lock` is the only place a PYTHON version lives: `uv <name> <version> [extras]`
  installs a tool, `uv-with <name> <package==version>` is a `--with` requirement of that tool (an
  adapter such as `harlequin-clickhouse` provides no executables, so it CANNOT be its own tool).
  Both print `PLAN UVTOOL` / `PLAN UVWITH`, and `verify.sh` asserts the version plus every adapter.
- Every artifact it is responsible for is announced with `plan KIND fields…`, printed as
  `PLAN<TAB>KIND<TAB>…`: `LINK dest src`, `GENERATED path`, `STATE path`, `EDITOR-COMMAND value`,
  `TOOL cmd`, `PLUGIN id path`, `INTEGRATION agent`, `GIT-INCLUDE path` (and `UVTOOL` when uv
  tools land).
- `dotfiles/verify.sh` runs `install.sh --dry-run` and then **checks the real machine against
  those lines**: symlink targets (a plain copy is drift — `git pull` would not reach it), tools
  (herdr/fresh/zsh/git/jq are failures, everything else warns), plugin registrations, agent-state
  integrations, the gitconfig include, the seeded sidebar `state.json` keys, and the keymap
  itself (binding count, `herdr config check`, and every `scripts/*.sh` the keymap runs must
  exist in this repo). Exit 1 means the machine is not what the repo describes.
- Therefore: **never add a link/tool/plugin to the installer without a `plan` line, and never
  add a check to `verify.sh` that duplicates one** — the plan IS the checklist, so the two
  cannot drift. `docs/install-herdr-ide.sh` ends with `verify.sh` as its gate; a half-configured
  machine can no longer report a successful install.
- `verify.sh` is read-only and safe to run any time (`--quiet` = problems only, `--plan` = the
  checklist). Use it instead of hand-checking after a pull.
- **Windows has a HOST bootstrap, not a second installer.** `docs/install-herdr-ide.ps1` only does
  what WSL cannot do for itself (distro check, `winget` Windows Terminal + Nerd Font, the
  `herdr.cmd` launcher, unbinding the two Windows Terminal chords) and then runs
  `docs/install-herdr-ide.sh` inside the distro. Never duplicate package/config logic there: the
  environment is the WSL one, and `verify.sh` is what proves it.
- **Every managed config is a whole-file symlink, `~/.zshrc` included** — the point is one identical
  environment per machine, so an existing file is backed up (`.dotfiles-backup/<ts>/`) and replaced.
  The one sanctioned escape hatch is `~/.zshrc.local`, which the managed rc sources above the
  highlight plugins and the installer never touches: SDK paths, work aliases and a different prompt
  live there. `--no-zshrc` leaves `~/.zshrc` alone entirely. `~/.gitconfig` is the only config that
  gets an additive edit (`include.path`).

## Verify before you finish

- Shell: `bash -n` and `shellcheck` on every touched script (`dotfiles/**/*.sh`, `dotfiles/herdr-plugins/ide/bin/*`,
  `docs/install-herdr-ide.sh`). The zsh hook (`shell/agent-title.zsh`) gets `zsh -n`.
- Rust (only if `plugins/herdr-sidebar` changed): from `plugins/herdr-sidebar`, run
  `cargo build --release && cargo test && cargo clippy -- -D warnings`.
- Installer: `dotfiles/install.sh --dry-run` and `dotfiles/verify.sh` are read-only and expected after a
  change to this layer. Do not run the full install, and do not run `dotfiles/install.sh` on the user's
  machine unless they ask.
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
- **`uv tool install <adapter>` fails: an adapter has no executables.** `harlequin-clickhouse` is a
  plugin for harlequin, so it must be `--with 'harlequin-clickhouse==…'` on the harlequin install;
  installing it alone errors with "No executables are provided by package". Its presence is only
  visible through `uv tool list --show-with`, so the skip test reads
  `uv tool list --show-extras --show-with` (version, extras and adapters in one listing) and
  `verify.sh` checks the adapter as its own artifact. Verified live on uv 0.12.8.
- **Windows Terminal's `settings.json` is JSONC, and PowerShell 5.1's `ConvertFrom-Json` rejects
  every bit of it** (`//` and `/* */` comments, trailing commas). `docs/install-herdr-ide.ps1` strips
  them with a string-aware scanner before parsing (a `//` inside a URL must survive), removes the
  `Ctrl+Shift+A` / `Ctrl+Tab` keybinding objects, and writes the file back with
  `[IO.File]::WriteAllText(..., UTF8Encoding($false))`. The rewrite is plain JSON, so the file's
  comments are gone — hence the timestamped backup next to it, and `-NoTerminalSettings` to skip.
- **Replacing `~/.zshrc` is deliberate: one environment everywhere.** A machine's own rc (oh-my-zsh,
  Go/Android `PATH`s, work aliases) must therefore MOVE its machine-specific lines into
  `~/.zshrc.local` before the first install, or they end up unsourced in
  `~/.dotfiles-backup/<ts>/.zshrc`. The overlay is sourced ABOVE the highlight plugins because
  zsh-syntax-highlighting has to be last, so a `.zshrc.local` that sources its own highlighting copy
  breaks that order (drop it there, not here). `--no-zshrc` skips the replacement.
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
- **The rail's agent name comes from `ide-agent-title`'s table, not from herdr.** herdr only
  auto-detects the agents it ships an integration for (`herdr integration status`) and reports
  its own id for those; every other CLI is invisible until something sets the pane's
  `display_agent` metadata, which `bin/ide-agent-title` does (`--display-agent`) alongside the
  border title from the typed command, and which herdr-sidebar's `$agents` token prefers
  (`src/agent_tokens.rs`). So `AGENT_OF` at the top of `bin/ide-agent-title` is the ONLY place
  to add an agent — a missing entry means the rail shows nothing (user-reported with
  CodeBuddy, `codebuddy` / npm `@tencent-ai/codebuddy-code`). It fires per TYPED command, so an
  agent already running keeps its nameless state until it is run again; naming the pane by hand
  with `herdr pane report-metadata <pane> --source dotfiles-ide-agent --display-agent <name>
  --title "◆ NAME"` fixes a live pane immediately (verified). Do not add a second table or a
  process-name detector elsewhere.
- **CodeBuddy's agent state comes from ITS hooks; herdr has no integration for it.** herdr's
  `integration install` targets stop at its own 18 agents, and it never detects the `codebuddy`
  process, so a pane running it was `·` unknown forever. `config/codebuddy/hooks/herdr-agent-state.sh`
  reports `idle` / `working` / `blocked` / release through `herdr pane report-agent`, and
  `install.sh`'s `codebuddy_hooks` merges the seven hook entries into
  `~/.codebuddy/settings.json` (Claude-Code-compatible schema: `hooks.<Event>[].hooks[].command`,
  `timeout` in SECONDS). Three rules bit us live: `--source` must NOT start with `herdr:` (reserved;
  the report is answered `ok` and dropped), `pane report-agent` is dropped on a pane that does not
  actually host an agent process, and `--seq` must strictly increase forever (the script keeps a
  nanosecond counter per pane under `~/.local/state/herdr-codebuddy/`). Hooks must print nothing and
  exit 0 — stdout is fed to the model and exit 2 blocks the tool — and CodeBuddy snapshots settings
  at startup, so reinstalling the hook needs a NEW `codebuddy`, not a `/reload`.
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
  **pi first**, then claude, codex, gemini, codebuddy, cursor-agent, opencode, aider, qwen,
  kimi, grok, droid — typing a missing binary into a pane leaves a dead shell that reads as
  "the IDE is broken" (user-reported on a fresh machine). Neither installed → `none`, so no
  Agent column. Get the config dir from the CLI, not a hardcoded `~/.config/herdr`: a user
  with `XDG_CONFIG_HOME` set has a different path.

## Commits

Never commit unless the user asks. The user makes the commits. When they do, fill in the `*uncommitted*`
rows in `docs/herdr-ide.md` with the hash.
