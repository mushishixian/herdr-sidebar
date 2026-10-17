# herdr IDE on WSL: how it fits together

A VS Code-shaped terminal IDE inside WSL (Ubuntu): **herdr** is the window
manager, the patched **herdr-sidebar** (this repo) is the explorer / search /
source control, **Fresh** is the editor, and a coding agent (Claude Code) sits
on the right. Nothing in herdr or Fresh themselves is patched: everything is
plugins, configuration, and this fork of herdr-sidebar.

```
 Windows:  Windows Terminal (or WezTerm) + a Nerd Font, opening straight into WSL
 WSL:      zsh ─ herdr ─┬─ Sidebar   herdr-sidebar fork (this repo)
                        ├─ Fresh     editor, one daemon per project per workspace
                        ├─ Terminal  shell under the editor
                        └─ Agent     claude, pinned on the right

   ┌─────────┬──────────────────────┬───────────┐
   │ Sidebar │ Fresh                │ Agent     │
   │ explorer│                      │ (claude)  │
   │ search  ├──────────────────────┤           │
   │ git     │ Terminal             │           │
   └─────────┴──────────────────────┴───────────┘
```

## Where everything lives

| Piece | Location | Kind |
|---|---|---|
| herdr-sidebar fork | `~/code/herdr-sidebar`, branch `fresh-inline-editor` (this repo) | Rust herdr plugin, linked with `herdr plugin link` |
| Dotfiles | `~/code/dotfiles` (local git repo, **no remote**: back it up yourself) | configs + scripts |
| dotfiles installer | `~/code/dotfiles/install.sh` | apt, herdr, Fresh, symlinks, plugins |
| Sidebar build helper | `~/code/dotfiles/herdr-sidebar/build.sh` + `000N-*.patch` | rebuilds the fork (clones + applies patches on a new machine) |
| IDE plugin `dotfiles-ide` | `~/code/dotfiles/herdr-plugins/ide` | bash herdr plugin (layout, terminal toggle, Ctrl+click → Fresh) |
| herdr config | `~/code/dotfiles/config/herdr/config.toml` → `~/.config/herdr/config.toml` | symlink |
| Fresh config | `~/code/dotfiles/config/fresh/config.json` → `~/.config/fresh/config.json` | symlink |
| Fresh plugins | `~/code/dotfiles/config/fresh/plugins/*.ts` → `~/.config/fresh/plugins/` | one symlink per file |
| Sidebar settings | `~/.local/state/herdr/plugins/herdr-sidebar/state.json` | seeded from `dotfiles/herdr-sidebar/state.json` |
| OpenCommit | `~/.local/bin/oco` (`npm install -g --prefix ~/.local opencommit`) | npm CLI |
| One-shot installer | `docs/install-herdr-ide.sh` (this folder) | wraps all of the above |

## Install

```bash
# 1. Put the dotfiles repo at ~/code/dotfiles (copy it over, it has no remote)
# 2. Copy this repo to ~/code/herdr-sidebar (branch fresh-inline-editor). Its
#    `origin` is upstream, so a fresh clone would NOT contain these commits.
cd ~/code/herdr-sidebar
docs/install-herdr-ide.sh            # --help for options (--lsp, --no-packages, --no-claude, --no-oco, --dotfiles DIR)
```

What it does, in order:

1. Warns if Fresh or herdr replaced their config symlink with a plain copy
   (see *Gotchas*) and shows the diff so you can merge it back first.
2. Runs `~/code/dotfiles/install.sh` with `HERDR_SIDEBAR_SRC` pointing at this
   checkout: apt packages (incl. `rustc cargo jq`), herdr + Fresh official
   installers, all symlinks, `build.sh` (cargo build of this repo), links
   `dotfiles-ide`, seeds the sidebar settings, installs herdr's Claude Code hook.
3. Makes sure herdr's registered `herdr-sidebar` is **this** checkout (re-links
   it if it points elsewhere or is the unpatched GitHub install).
4. Installs OpenCommit into `~/.local`.
5. Verifies the actions, Fresh plugins and keys are visible, reloads herdr.

Afterwards, once:

- `oco setup`: choose the AI provider and key for the AI-commit key.
- Restart running Fresh daemons so new plugins/keys load (see *Updating*).
- Windows Terminal: remove its default **Ctrl+Shift+A → Select all**
  binding (Settings → Actions), or the AI-commit chord never reaches herdr.
- Start `herdr`, open a project pane, press `ctrl+b i` for the IDE layout.

## Keys

### herdr, global (`config/herdr/config.toml`)

| Key | Does |
|---|---|
| `ctrl+b i` | IDE layout for the current pane's project (focuses Fresh if already built) |
| ``ctrl+b ` `` | Toggle the terminal under Fresh (focus / collapse / reopen) |
| `ctrl+b a` | Toggle the sidebar |
| `ctrl+b f` | Quick Open in the sidebar |
| `ctrl+b /` | Search in files |
| `ctrl+b d` | Source Control view |
| `ctrl+q` | File-search popup (fuzzy find, opens in Fresh/preview) |
| `ctrl+alt+c` | Git: switch branch (popup; type a new name to create) |
| `ctrl+alt+r` | Git: rename current branch (popup) |
| `ctrl+alt+x` | Git: merge a branch into the current one (popup) |
| `ctrl+alt+p` | Git: pull (background, progress toasts) |
| `ctrl+alt+k` | Git: push (background, progress toasts) |
| `ctrl+shift+a` | Git: `add -A` + commit with an OpenCommit message |
| `ctrl+b alt+g` | lazygit popup |
| `ctrl+b t` | Scratch terminal popup |
| `ctrl+b ?` | Every herdr binding |

Git keys act on the repository of the **focused pane's** current directory.

**Shortcut toasts:** every custom shortcut (herdr and Fresh) shows a small
toast with its name and key, e.g. "Switch branch / Ctrl+Alt+C", as a reminder.
The key is looked up live from the config files, so rebinding needs no code
change. herdr-sidebar toasts only when started as a plugin action
(`HERDR_PLUGIN_ACTION_ID` set; `src/shortcut_tip.rs`), so hooks never toast;
dotfiles-ide uses `tip` in `lib.sh`; the two `popup` keys run
`herdr notification show` before their command.

### Fresh (`config/fresh/config.json`)

| Key | Does |
|---|---|
| `ctrl+alt+o` | Git history of the selected lines (herdr popup, `git log -L`) |
| `ctrl+alt+y` | Copy the selected text (cursor line if nothing selected) |
| `ctrl+alt+l` | Copy as reference: `path:12-20` + fenced code block |
| `ctrl+alt+a` | Paste that reference into this tab's Agent pane (not submitted) and focus it |
| `ctrl+tab` | Jump to the previously used open tab (again = back); never opens files |

The first four are also in Fresh's **Selection** menu, and all five in the
command palette (`ctrl+p`). Ctrl+Tab is bound with `"when": "normal"` (editing
only). Windows Terminal binds Ctrl+Tab to its own "next tab" by default:
unbind it there (see *Gotchas*). Shift+Tab is Fresh's normal un-indent.

### Sidebar

`1` / `2` / `3` Explorer / Search / Source Control (`ctrl+1..3` from a text
field), `ctrl+p` Quick Open, `ctrl+f` search, `m` or **right-click** context
menu, `s` Settings, `b` hide, `c` change folder, `q` quit.

## How each piece works

### IDE layout: `dotfiles-ide` plugin

- **Pane identity is by label**, so it survives herdr restarts: `Fresh`,
  `Terminal`, `Agent`; `Sidebar` belongs to herdr-sidebar. The Fresh pane also
  carries a `fresh_daemon=<name>` metadata token.
- `bin/herdr-ide` (action `layout`) splits the current tab into the layout
  above. Started from an agent pane, that agent becomes the right column; from
  a shell, the shell becomes the Terminal and a new agent opens (agent command:
  plugin config `agent` file, else `$IDE_AGENT`, else `claude`; `none` = no column).
- `bin/fresh-ide DIR` runs `fresh -a ide-<sha1(project root)[:10]>`: **one Fresh
  daemon per project per herdr workspace**. The first workspace on a project
  gets `ide-<hash>`, the next `ide-<hash>-s2`, `-s3`, and so on; a workspace
  keeps the daemon its Fresh pane already has (`ide_daemon_for` in `lib.sh`).
  The editor survives its pane closing and other tools can hand files to it.
  Short names avoid the 108-byte socket-path limit. `fresh-ide` exports
  `FRESH_IDE_DAEMON` so the editor plugin knows its own daemon.
  It also labels the pane and sets `herdr pane input --right-click pane`, so a
  plain right-click reaches Fresh (its tab and file-tree menus); right-click on
  the pane **border** still opens herdr's pane menu.
- `bin/herdr-ide-term` (action `toggle-terminal`) moves the editor/terminal
  split ratio instead of zooming, so the side columns never disappear.
- **Pane sizes are remembered per project**, shared by all of its workspace
  daemons (the daemon name without its `-sN` suffix). herdr has no hookable resize
  event, so `bin/ide-save-sizes` runs on `pane.focused` / `tab.focused` and
  records, for every tab holding a Fresh pane, the Agent-column ratio and the
  Fresh/Terminal ratio under that project's key in
  `~/.local/state/herdr/plugins/dotfiles-ide/sizes.json` (a collapsed terminal,
  ≤ 5 rows, is not saved). `herdr-ide` re-applies them after building a layout
  and `herdr-ide-term` reopens the terminal at the saved ratio, via
  `layout.set_split_ratio`. Splits are found by geometry; a split id such as
  `split_2_01` encodes its tree path (0 = first child). The sidebar's own
  column is left to herdr-sidebar.
- `bin/fresh-open FILE[:LINE[:COL]] | file://URL | --link` sends a file to the
  nearest `Fresh` pane (this tab, then this workspace — never another
  workspace's, even on the same project: that workspace has its own daemon).
  A live-diff request (sidebar git clicks) is written per daemon, as
  `live-diff-request-<daemon>` in `$XDG_STATE_HOME/fresh-ide`, so only the
  editor the file went to consumes it. It is the sidebar's custom editor (`editor-command.txt`) and
  the target of the `file-link` link handler, so **Ctrl+click on `file://`
  links** (rg, delta, eza, fd) opens Fresh
  at that line.
- `bin/fresh-to-agent TEXT` pastes text into the `Agent` pane **of the Fresh
  pane's tab only** (never another tab's agent session) via the socket method
  `pane.send_input`, which uses bracketed paste so newlines don't submit.
- `bin/herdr-rpc METHOD JSON` is a tiny Python client for socket methods the
  CLI does not wrap (`pane.focus` by id, `pane.send_input`).

### herdr-sidebar fork (this repo)

Upstream: `alexarthurs/herdr-sidebar`, forked at `v0.15.1`. Commits on
`fresh-inline-editor`:

| Commit | Feature | Mechanism |
|---|---|---|
| a8e9cd6 | **Editor opens: inline** | Settings row `custom_editor_inline`: a click runs the editor command (`fresh-open`) in the background instead of opening a new herdr tab, so the sidebar and agent columns never move. |
| 22756b9 | **Branch switch / rename / merge popups** | Actions `git-switch-branch`, `git-rename-branch`, `git-merge-branch` → `--git-popup-open <op>` resolves the focused pane's cwd and opens `--git-popup <op>` via `plugin.pane.open` placement `popup`. Rename uses `git branch -m` (never `-M`); merge uses `--no-edit` and offers `a` to abort on conflicts. |
| 2c0912a | **Plain right-click menu + Cut/Copy/Paste/Duplicate** | The sidebar calls `pane.input.set right_click=pane` for its own pane. The file clipboard is a file in the plugin state dir, so copy in one tab and paste in another. Copies never overwrite (`name copy`, `name copy 2`); cut refuses an existing name. |
| 9712dd7 | **Popups close on focus loss** | `FocusLost` events plus, while a popup is open, polling `pane.list` every 200 ms; a failed poll never closes anything. Typed prompts, Quick Open and Search stay open. |
| e437091 | **herdr's pane items in the sidebar menu** | Rename pane, Split right/down, Zoom, Use herdr right-click menu, Close pane, through the socket API, since the sidebar's plain right-click hides herdr's menu. |
| 01dea4e | **Line-history popup** | Entrypoint `git-line-history` (`--git-popup history`), range from `HERDR_SIDEBAR_HISTORY_FILE` / `HERDR_SIDEBAR_HISTORY_LINES`; opened by the Fresh plugin below. |
| 60dd2a3 | **File-search popup** | Action `file-search` (`--file-popup-open`) → popup `--file-popup`: fuzzy file list (skips `.git`, honours `.gitignore`), opens the pick in the caller's tab. |
| *uncommitted* | **Git pull / push / AI commit** | `src/git_action.rs`, actions `git-pull`, `git-push`, `git-commit-ai` (`--git-pull/--git-push/--git-commit`). A per-repo OS file lock prevents overlapping runs; progress and results are herdr toasts (`notification.show`). Git runs with `GIT_TERMINAL_PROMPT=0` and null stdin so a credential prompt fails instead of hanging. |

**AI commit** (`ctrl+shift+a`): `git add -A` → nothing staged? toast
"Nothing to commit" → `oco --yes` in the repo root with `OCO_GITPUSH=false`
(otherwise oco asks to push after committing), `NO_COLOR=1`, null stdin, a 120 s
timeout → toast `Committed: <subject>`. An unconfigured oco starts its setup
wizard and crashes without a TTY (`ERR_TTY_INIT_FAILED`); that is reported as
"run `oco setup`". On any failure the changes stay staged.

Sidebar defaults seeded by the installer (`dotfiles/herdr-sidebar/state.json`):
VS Code colours, git footer, auto-open, follow the neighbour pane's folder,
preview in a pane (a git-change diff opens beside the sidebar inside the IDE tab, so the Agent column stays visible; Esc/q closes it), editor on click, editor inline.

### Fresh plugins (`dotfiles/config/fresh/plugins`)

- **`git_line_history.ts`**: takes the selection's line range (a selection ending
  at column 0 of a later line excludes that line) and runs
  `herdr plugin pane open --plugin herdr-sidebar --entrypoint git-line-history
  --placement popup --env HERDR_SIDEBAR_HISTORY_FILE=… --env HERDR_SIDEBAR_HISTORY_LINES=start,end`.
  History is from the saved file, so it warns on unsaved buffers.
- **`selection_share.ts`**: the copy / reference / send-to-agent commands. The
  path is relative to Fresh's cwd (the project root). Send-to-agent spawns
  `$DOTFILES_IDE_BIN/fresh-to-agent` (default
  `~/code/dotfiles/herdr-plugins/ide/bin`) and caps text at 200 000 chars.

## Updating

```bash
# rebuild the sidebar after changing this repo, restart its panes
(cd plugins/herdr-sidebar && cargo build --release) && herdr plugin action invoke herdr-sidebar.redeploy

# herdr config edits
herdr server reload-config

# Fresh config or plugin edits: daemons load them only at start
fresh --cmd daemon list
fresh --cmd daemon kill ide-xxxxxxxxxx      # save first; then run fresh-ide in the Fresh pane

# keep the dotfiles patch set in sync with this branch (commit first)
rm ~/code/dotfiles/herdr-sidebar/*.patch
git format-patch "$(git merge-base origin/main HEAD)" -o ~/code/dotfiles/herdr-sidebar

# rebase the fork onto the newest upstream release (rewrites the patches)
~/code/dotfiles/herdr-sidebar/build.sh --update
```

The dotfiles patch set currently stops at 01dea4e: the file-search popup and
the git pull/push/AI-commit work are only in this checkout. A new machine
that runs `build.sh` without this repo gets the older feature set until the
patches are re-exported (above). `docs/install-herdr-ide.sh` avoids that by
building this checkout directly.

## Gotchas (verified)

- **Fresh replaces symlinked `config.json`** when its settings UI saves, turning
  the dotfiles symlink into a plain copy; later dotfiles edits then silently do
  nothing. Check `ls -la ~/.config/fresh`, merge the copy's changes into
  dotfiles, rerun the installer (it backs up and re-links). New Fresh plugins
  also need their own symlink: rerun the installer.
- **herdr's right-click pane menu cannot be extended by plugins.** Its items are
  hard-coded (`src/client/shell/context_menu.rs`); the manifest `contexts` field
  is accepted but unused for menus. Hence the Fresh keys + Selection menu.
- **Fresh 0.5.3 never reports right/middle clicks to plugins**, and has no
  right-click menu in the text area (only tabs, `+`, file explorer).
- **Fresh's daemon mode drops the kitty keyboard protocol.** Plain `fresh`
  requests it (`keyboard_disambiguate_escape_codes`), but `fresh -a` (server/
  client) never does (`terminal_setup_sequences`, Fresh 0.5.3), so herdr encodes
  keys the legacy way and Ctrl+Tab arrives as Tab. `fresh-ide` writes the
  request (`ESC [>1u`) to the pane itself shortly after Fresh enters its
  alternate screen (the flags are per screen, so it must come after).
- **Fresh's daemon draws one editor to every attached client**, so two panes
  on one daemon mirror each other; that is why each herdr workspace gets its
  own daemon.
- **`pane.send_text` sends raw bytes** (a newline submits an agent prompt);
  `pane.send_input {text}` bracket-pastes when the app enabled it.
- **Windows Terminal swallows some chords**: Ctrl+Tab (next tab), Ctrl+Shift+A (select all),
  Ctrl+Shift+C/V (copy/paste); that is why the git keys use Ctrl+Alt.
  Terminals send Ctrl+H as Backspace, so line history is on `ctrl+alt+o`, not `ctrl+alt+h`.
- **`npm install -g` needs `--prefix ~/.local`**: the global prefix is root-owned.
- **Running sidebars keep the old binary** until redeployed
  (`herdr plugin action invoke herdr-sidebar.redeploy`).
- Agent shells set `NO_COLOR=1`; a herdr server started from one renders
  monochrome. Start herdr from a normal shell.

## Troubleshooting

| Symptom | Check |
|---|---|
| A Fresh key does nothing | `fresh --cmd config show \| grep selection_` (config linked?), `ls -la ~/.config/fresh/plugins`, restart the daemon |
| `ctrl+shift+a` does nothing | Windows Terminal's Select-all binding; `herdr plugin action invoke herdr-sidebar.git-commit-ai` to test without the key |
| "OpenCommit is not configured" toast | run `oco setup` |
| "No Agent pane in this tab" | the tab has no pane labelled `Agent`; run `ctrl+b i` |
| Sidebar lacks a new feature | `herdr plugin list --json` → `plugin_root` must be this checkout; rebuild + redeploy |
| Actions missing in `herdr plugin action list` | manifest error: `herdr plugin log list --plugin herdr-sidebar` |

More internals (herdr behaviour, heartbeat liveness, preview tabs, release
flow) are in the repo's `AGENTS.md`.
