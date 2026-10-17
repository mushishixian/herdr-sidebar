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
| Dotfiles | `dotfiles/` in this repo (branch `fresh-inline-editor`; backed up wherever this branch is pushed) | configs + scripts |
| dotfiles installer | `dotfiles/install.sh` | apt, herdr, Fresh, symlinks, plugins |
| Sidebar build helper | `dotfiles/herdr-sidebar/build.sh` | builds this checkout (`plugins/herdr-sidebar`) and points herdr's `herdr-sidebar` registration at it |
| IDE plugin `dotfiles-ide` | `dotfiles/herdr-plugins/ide` | bash herdr plugin (layout, terminal toggle, Ctrl+click → Fresh) |
| herdr config | `dotfiles/config/herdr/config.toml` → `~/.config/herdr/config.toml` | symlink |
| Fresh config | `dotfiles/config/fresh/config.json` → `~/.config/fresh/config.json` | symlink |
| Fresh plugins | `dotfiles/config/fresh/plugins/*.ts` → `~/.config/fresh/plugins/` | one symlink per file |
| Sidebar settings | `~/.local/state/herdr/plugins/herdr-sidebar/state.json` | seeded from `dotfiles/herdr-sidebar/state.json` |
| OpenCommit | `~/.local/bin/oco` (`npm install -g --prefix ~/.local opencommit`) | npm CLI |
| One-shot installer | `docs/install-herdr-ide.sh` (this folder) | wraps all of the above |

### Repository layout

```
herdr-sidebar/                 this repo (branch fresh-inline-editor)
├── plugins/herdr-sidebar/     Rust plugin: explorer, search, source control (upstream-shaped, builds standalone)
├── dotfiles/                  IDE layer: install.sh, herdr/Fresh/lazygit/shell configs, dotfiles-ide plugin, build.sh
└── docs/                      this document and install-herdr-ide.sh
```

`plugins/herdr-sidebar` never references `dotfiles/`, so upstream merges stay clean.

## Install

On a new machine, clone this repo's `fresh-inline-editor` branch from wherever it
is pushed (your fork, not `alexarthurs/herdr-sidebar`: `origin` there is upstream
and would NOT contain these commits). The dotfiles live in its `dotfiles/`
directory, so there is no second repo to fetch.

```bash
git clone -b fresh-inline-editor <your fork URL> ~/code/herdr-sidebar
cd ~/code/herdr-sidebar
docs/install-herdr-ide.sh            # --help for options (--lsp, --no-packages, --no-claude, --no-oco)
```

What it does, in order:

1. Drift check: warns if Fresh or herdr replaced their config symlink with a plain
   copy (see *Gotchas*) and shows the diff so you can merge it into `dotfiles/config/` first.
2. Runs `dotfiles/install.sh`: apt packages (incl. `rustc cargo jq`), herdr + Fresh
   official installers, all symlinks, `dotfiles/herdr-sidebar/build.sh` (builds
   `plugins/herdr-sidebar` from this checkout and points herdr's `herdr-sidebar`
   registration at it, replacing any other registration), links `dotfiles-ide`,
   seeds the sidebar settings, installs herdr's Claude Code hook.
3. Installs OpenCommit into `~/.local`.
4. Verifies the actions, Fresh plugins and keys are visible, that the `herdr-sidebar`
   and `dotfiles-ide` registrations both point into this repo, then reloads herdr.

Afterwards, once:

- `oco setup`: choose the AI provider and key for the AI-commit key.
- Restart running Fresh so new plugins/keys load: `dotfiles/herdr-plugins/ide/bin/fresh-restart --all`
  from the repo root (see *Updating*).
- Windows Terminal (and Pebrel): remove its default **Ctrl+Shift+A → Select all** and **Ctrl+Tab → next tab**
  bindings (Settings → Actions), or the AI-commit and recent-tab chords never reach herdr and Fresh.
- Start `herdr`, open a project pane, press `ctrl+b i` for the IDE layout.

### Migrating from the old `~/code/dotfiles`

Machines set up before the merge still have the old checkout, and their symlinks
point into `~/code/dotfiles`. Pull this branch into `~/code/herdr-sidebar` and rerun
`docs/install-herdr-ide.sh`. Its `link()` re-points any symlink that points elsewhere
(the replaced files go to `~/.dotfiles-backup/<timestamp>/`), re-links the
`dotfiles-ide` plugin when its registration points at an old root, and rewrites
`editor-command.txt` on every run. After that the old `~/code/dotfiles` checkout is
unused and can be archived. If Fresh's settings screen had left
`~/.config/fresh/config.json` as a plain copy, the drift check shows its diff: merge
that into `dotfiles/config/fresh/config.json` before rerunning, because the re-link
moves the copy aside.

## Keys

### herdr, global (`dotfiles/config/herdr/config.toml`)

| Key | Does |
|---|---|
| `ctrl+b i` | IDE layout for the current pane's project (focuses Fresh if already built) |
| ``ctrl+b ` `` | Toggle the terminal under Fresh (focus / collapse / reopen) |
| `ctrl+b a` | Toggle the sidebar |
| `ctrl+b f` | Quick Open in the sidebar |
| `ctrl+b /` | Search in files |
| `ctrl+b d` | Source Control view |
| `ctrl+q` | File-search popup (fuzzy find, opens in Fresh/preview) |
| `ctrl+alt+f` | Search in files popup (file contents, live preview; opens at the matching line) |
| `ctrl+alt+e` | Replace in files popup (the search popup with a Replace row: Ctrl+R replaces the selected item, Alt+Enter replaces all, after a y/N prompt) |
| `ctrl+alt+c` | Git: switch branch (popup; type a new name to create) |
| `ctrl+alt+r` | Git: rename current branch (popup) |
| `ctrl+alt+x` | Git: merge a branch into the current one (popup) |
| `ctrl+alt+p` | Git: pull (background, progress toasts) |
| `ctrl+alt+k` | Git: push (background, progress toasts) |
| `ctrl+shift+a` | Git: stage all + OpenCommit message into Source Control, then commit |
| `ctrl+b alt+g` | lazygit popup |
| `ctrl+b t` | Scratch terminal popup |
| `ctrl+b ?` | Every herdr binding |

Git keys act on the repository of the **focused pane's** current directory.

Two popups search from the focused pane's project. **Ctrl+Q** is quick open: a
fuzzy file list, and Enter opens the pick in Fresh or the preview. **Ctrl+Alt+F**
searches file contents: a query box with Match Case, Whole Word and Regex toggles
(Alt+C / Alt+W / Alt+R) on the left, grouped results, and a syntax-highlighted
preview of the selected match on the right. Enter opens the match at its line in
Fresh. Narrow terminals drop the preview and keep the list.
**Ctrl+Alt+E** is the same popup in replace mode: a chevron opens a Replace row
under the query, and Tab moves between the two inputs. Ctrl+R replaces the selected
match (or, on a file header, every match in that file after a `y` confirmation);
Alt+Enter replaces every match in the project after a `y`. Open editor buffers see
the change on disk as an external change.

**Shortcut toasts:** every custom shortcut (herdr and Fresh) shows a small
toast with its name and key, e.g. "Switch branch / Ctrl+Alt+C", as a reminder.
The key is looked up live from the config files, so rebinding needs no code
change. herdr-sidebar toasts only when started as a plugin action
(`HERDR_PLUGIN_ACTION_ID` set; `src/shortcut_tip.rs`), so hooks never toast;
dotfiles-ide uses `tip` in `lib.sh`; the two `popup` keys run
`herdr notification show` before their command.

### Fresh (`dotfiles/config/fresh/config.json`)

| Key | Does |
|---|---|
| `ctrl+alt+o` | Git history of the selected lines (herdr popup, `git log -L`) |
| `ctrl+alt+y` | Copy the selected text (cursor line if nothing selected) |
| `ctrl+alt+l` | Copy as reference: `path:12-20` + fenced code block |
| `ctrl+alt+a` | Paste that reference into this tab's Agent pane (not submitted) and focus it |
| `ctrl+tab` | Jump to the previously used open tab (again = back); never opens files |
| `alt+c` | Merge conflict: accept **current** (the block at the cursor, or the next one) |
| `alt+i` | Merge conflict: accept **incoming** |
| `alt+m` | Merge conflict: accept **both** (current, then incoming) |
| `alt+d` | Merge conflict: next conflict (past the last: next conflicted file) |
| `alt+shift+d` | Merge conflict: previous conflict (wraps to the last) |

The first four are also in Fresh's **Selection** menu, and all five in the
command palette (`ctrl+p`). Ctrl+Tab is bound with `"when": "normal"` (editing
only). Windows Terminal binds Ctrl+Tab to its own "next tab" by default:
unbind it there (see *Gotchas*). Shift+Tab is Fresh's normal un-indent.

The conflict keys are free in Fresh's default keymap: Alt+1/2/3 are bookmarks,
Alt+N/P find-selection, and Alt+Shift+N/P sidebar sections, so they were not
reused. In a file without conflicts the conflict keys only show "No merge
conflicts in this file".

**Resolving merge conflicts.** Two resolvers exist. The inline one (this
config, `plugins/selection_share.ts`, "Inline merge conflicts") works in the
normal buffer like VS Code: each block is tinted (current green, incoming blue,
diff3 base grey), an action line sits above each `<<<<<<<`, and the keys above
accept a side. Accepting moves to the next unresolved conflict. When the file
is clean it is saved, and the next conflicted file from `git diff
--diff-filter=U` opens at its first conflict. Stage files in Source Control
and commit when none remain. Fresh's built-in **Merge: Start Resolution**
(`merge_conflict.ts`) is the alternative: a three-panel view (ours, theirs,
editable result). Both coexist; the inline resolver never clears the built-in
one's decorations. A conflicted file opened from Source Control skips the Live
Diff (see AGENTS.md, *SCM changed-file routing*).

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
- **Agent pane title.** The Agent pane's border shows the agent it runs: `◆ CLAUDE`,
  `◆ CODEX`, and so on, taken from the command you type there (`claude -r`,
  `codex resume xxxx`, `npx @openai/codex@latest`, `env X=1 claude`, `~/.local/bin/claude`
  all work). `bin/ide-agent-title` parses the command without `eval` and reports it through
  `pane report-metadata --title`. A metadata title is drawn in preference to the manual
  label, so the pane stays labelled `Agent` for the scripts while the border reads
  `◆ CLAUDE`. When the agent exits back to the shell prompt the title clears and the
  border reads `Agent` again. The table of known agents (claude, codex, gemini, opencode,
  aider, pi, amp, cursor-agent, copilot, qwen, crush, goose, kimi, droid) is at the top of
  `bin/ide-agent-title`; add a line there to teach it another. The clearing needs the
  zsh hook `shell/agent-title.zsh`, which `herdr-ide` sources into the pane before starting
  the agent. An agent that was already running when the layout was built gets its title
  once, with no hook, so that title stays after it exits (clear it with
  `dotfiles/herdr-plugins/ide/bin/ide-agent-title --clear <pane>`). Panes created before
  this change get the hook after `ctrl+b i` (or by running
  `. ~/code/herdr-sidebar/dotfiles/herdr-plugins/ide/shell/agent-title.zsh` in the Agent
  pane once).
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

### Previews cover the Fresh pane

With the IDE layout, a sidebar preview opens **inside the Fresh pane** instead of
splitting a new pane. That covers pictures, Source Control commits, stashes and
branches, diffs (including deleted and binary files), and Enter on a file. `q` or Esc
(or Esc in the sidebar) closes the preview and brings Fresh back with all of its
buffers. Clicking a text file in the sidebar also closes the cover and opens the file
in Fresh.

How it works: `fresh-ide` runs a small host loop in the Fresh pane and stamps the pane
with the `hs-preview-host` token. When the sidebar wants a preview there, it writes a
request file under its state directory (`preview-host/<pane>.req`: the viewer, its
directory and its environment). The host claims the request by renaming it, then
suspends the Fresh client. The daemon keeps every buffer, so nothing is lost. The
viewer runs in the same pane, and when it exits the host clears the preview's pane
tokens and attaches Fresh again.

- Fresh panes started before this change have no host loop: run
  `dotfiles/herdr-plugins/ide/bin/fresh-restart --all` once.
- Tabs without a Fresh pane keep the old split or tab behaviour.
- `fresh-restart` skips a pane that a preview covers and says so
  (`a preview covers pane …; close it (q) and rerun`), because restarting it would type
  `fresh-ide` into the viewer.

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
| ea18341 | **Git pull / push (+ first AI commit), shortcut toasts** | `src/git_action.rs`, `src/shortcut_tip.rs`; actions `git-pull`, `git-push`, `git-commit-ai` (`--git-pull/--git-push/--git-commit`). Pull/push take a per-repo OS file lock; results are herdr toasts (`notification.show`); toasts carry the bound key, and only for plugin actions (`HERDR_PLUGIN_ACTION_ID`). Git runs with `GIT_TERMINAL_PROMPT=0` and null stdin so a credential prompt fails instead of hanging. |
| 2a64708 | **AI commit through Source Control** | `src/ai_commit.rs`: `ctrl+shift+a` writes a per-tab request; that tab's Source Control sidebar stages everything, gets the OpenCommit message (prepare-commit-msg hook mode), fills Message, and commits through the ✓ Commit path. |
| aedfbd6 | **Popup → editor caller context; AI-commit button animation** | Popups belong to no tab, so the inline editor is started with the CALLER pane's `HERDR_PANE_ID` / `HERDR_TAB_ID` / `HERDR_WORKSPACE_ID`. The ✓ Commit button shows a braille spinner with "AI generating…" / "Committing…" and a sweeping highlight band while an AI commit runs, and blocks a second commit. |
| 2c02d3e | **Search popup + line-aware open** | `src/search_popup.rs`; the engine moved to lib `src/content_search.rs` (shared with the sidebar Search view); preview highlighted with `syntax::highlight_lines`; `actions::open_in_editor_inline_at` (`{line}` placeholder, or `file:line` for `fresh` / `fresh-open`). |
| c5661c3 | **Replace popup + conflict routing** | Replace mode of the search popup (`HERDR_SIDEBAR_SEARCH_MODE=replace`, action `replace-popup`; `content_search::replace_in_text` / `apply_replacements`: engine-identical matching, `$1` expansion in regex mode, line endings / BOM / permissions kept, atomic write, root-confined). Conflicted (`!`) files open in the editor without Live Diff (`editor_live_diff`). |
| 261bccb | **Dotfiles folded into this repo** | The standalone `~/code/dotfiles` now lives in `dotfiles/`; the upstream patch set was dropped. `dotfiles/herdr-sidebar/build.sh` builds this checkout and points herdr's `herdr-sidebar` registration at it. |
| *uncommitted* | **Media opens in the preview; previews cover Fresh; Agent pane title** | `viewer::is_media_file` keeps pictures and videos out of the custom editor (Explorer click, Ctrl+Q popup; Source Control's `diff_opens_in_editor` media flag). Preview host: `fresh-ide` runs a host loop in the Fresh pane (token `hs-preview-host`); the sidebar writes `<sidebar state dir>/preview-host/<pane>.req`, the host claims it by rename, and the viewer runs there with `HERDR_SIDEBAR_PREVIEW_HOSTED=1`; `viewer::uncover_host` runs before the editor launches. Agent pane title: `ide-agent-title` + `shell/agent-title.zsh` (zsh preexec/precmd) → `pane report-metadata --title "◆ CLAUDE"`. |

**AI commit** (`ctrl+shift+a`): the action does not commit. It writes a per-tab
request (`src/ai_commit.rs`, under the sidebar's private runtime dir) for the focused
pane's repo and activates that tab's Source Control view. The Source Control sidebar
then stages everything (`stage_under`, nested-repo safe), asks OpenCommit for a message
and puts it in the Message box, then commits through the same path as the ✓ Commit
button about 0.9 s later. Nothing staged → toast "Nothing to commit".

The message comes from oco's `prepare-commit-msg` hook mode: a private temp dir holds a
`hooks/prepare-commit-msg` SYMLINK to oco's `cli.cjs` (it must be a symlink, since oco
loads `tiktoken_bg.wasm` next to itself), and `GIT_CONFIG_COUNT`/`core.hooksPath` points
git there. oco treats itself as the hook, writes the message into the file it is given
and never commits. It runs with `OCO_GITPUSH=false`, `NO_COLOR=1`, null stdin, and a 120 s
timeout. An unconfigured oco reports `ERR_TTY_INIT_FAILED` (shown as "run `oco setup`").
On failure the changes stay staged and the message box is left alone.

While it runs, the ✓ Commit button shows a braille spinner with "AI generating…" (then "Committing…")
and a sweeping highlight band. Clicking ✓ Commit during that just says "AI commit in progress…".

Sidebar defaults seeded by the installer (`dotfiles/herdr-sidebar/state.json`):
VS Code colours, git footer, auto-open, follow the neighbour pane's folder,
preview in a pane (a git-change diff opens beside the sidebar inside the IDE tab, so the Agent column stays visible; Esc/q closes it), editor on click, editor inline.

Pictures and videos never open in Fresh. An Explorer click, Enter, or a pick in the Ctrl+Q popup
opens them in the sidebar's preview, which draws them in the terminal; `q` or Esc closes it.
Pictures: png, jpg/jpeg, gif, webp, bmp, ico, tif/tiff. Videos (avi, flv, m4v, mkv, mov, mp4, mpeg,
mpg, webm, wmv) show their first frame and need `ffmpeg`. For the full-resolution file, use `m` →
"Open with Default App" (runs `xdg-open`; on WSL, `dotfiles/bin/wsl-open <file>` hands it to Windows).
A changed picture in Source Control opens the diff preview, not Fresh. The explicit "Open in Editor"
menu item still sends a picture to Fresh.

### Search / replace popups

- Entry points: `--search-popup-open` (action `search-popup`, `ctrl+alt+f`) and `--replace-popup-open`
  (action `replace-popup`, `ctrl+alt+e`). Both open the pane `--search-popup` (placement `popup`, 90%×85%,
  `src/search_popup.rs`); replace mode is `HERDR_SIDEBAR_SEARCH_MODE=replace`. Windows twins go through
  `herdr-sidebar-ensure.exe`.
- The query runs on a worker after a 300 ms debounce; each search has a generation counter and older
  results are dropped. The engine cannot cancel, so a superseded scan runs to completion.
- The preview window hits ±300 lines and is highlighted with `syntax::highlight_lines`, which returns one
  line per input line. (The old join-and-check code dropped ALL colors whenever the window ended on a blank
  line.)
- Enter opens the match at its line in THIS workspace's Fresh (`fresh-open` with `file:line`).
- Replace safety: Alt+Enter (replace all) and Ctrl+R on a file header ask `y/N` first. A single replace
  re-verifies that its line still matches at that byte range. Writes are atomic (temp file, rename), keep
  permissions, CRLF and BOM, and never leave the project root (symlinks are canonicalized).
- The sidebar's own Search view still has an inert Replace field; replacement lives only in the popup.
- Popup panes belong to no tab, so both popups (and Ctrl+Q, since `aedfbd6`) hand the editor
  the caller pane's `HERDR_PANE_ID` / `HERDR_TAB_ID` / `HERDR_WORKSPACE_ID`.

### Fresh plugins (`dotfiles/config/fresh/plugins`)

- **`git_line_history.ts`**: takes the selection's line range (a selection ending
  at column 0 of a later line excludes that line) and runs
  `herdr plugin pane open --plugin herdr-sidebar --entrypoint git-line-history
  --placement popup --env HERDR_SIDEBAR_HISTORY_FILE=… --env HERDR_SIDEBAR_HISTORY_LINES=start,end`.
  History is from the saved file, so it warns on unsaved buffers.
- **`selection_share.ts`**: the copy / reference / send-to-agent commands. The
  path is relative to Fresh's cwd (the project root). Send-to-agent spawns
  `$DOTFILES_IDE_BIN/fresh-to-agent` and caps text at 200 000 chars.
  `fresh-ide` exports `DOTFILES_IDE_BIN` (its own bin dir) and the Fresh daemon
  inherits it, so this finds the script wherever the repo is checked out. The
  fallback is `~/code/herdr-sidebar/dotfiles/herdr-plugins/ide/bin`.
- **`recent_tab`** (`ctrl+tab`, `when: normal`): switches to the previously used open file tab; again goes
  back. It never opens files.
- **Live diff on request**: polls `$XDG_STATE_HOME/fresh-ide/live-diff-request-<FRESH_IDE_DAEMON>` every
  400 ms. A request at most 15 s old runs `live_diff_vs_head` on the file it names. `fresh-open` writes
  these (sidebar clicks on git changes).
- **Inline merge conflicts** (namespace `inline-conflict`): scans on buffer activate, open, revert and
  modify, debounced 150 ms. Accepting a side is a deleteRange + insertText, then the cursor auto-jumps to
  the next unresolved block. When a file is clean it is saved, and the next file from
  `git diff --diff-filter=U` opens at its first conflict.

## Updating

```bash
# rebuild the sidebar after changing this repo, restart its panes
(cd plugins/herdr-sidebar && cargo build --release) && herdr plugin action invoke herdr-sidebar.redeploy

# herdr config edits
herdr server reload-config

# Fresh config or plugin edits: daemons load them only at start
dotfiles/herdr-plugins/ide/bin/fresh-restart --all   # graceful; unsaved buffers come back (hot exit), run from the repo root
# by hand, if fresh-restart is unavailable: fresh --cmd daemon list; fresh --cmd daemon kill <name>; then fresh-ide in that pane

# pull this branch (or rerun docs/install-herdr-ide.sh), then rebuild and re-point herdr at this checkout
git pull
dotfiles/herdr-sidebar/build.sh

# take upstream herdr-sidebar changes (origin is alexarthurs/herdr-sidebar), then rebuild the same way
git fetch origin && git merge origin/main
dotfiles/herdr-sidebar/build.sh
```

`fresh-restart` with no argument restarts only this tab's Fresh (it is also the dotfiles-ide action
`restart-fresh`).

There is no patch set to maintain: the fork and the dotfiles are one repo, so an upstream update is a
normal git merge and `build.sh` builds whatever is checked out. A machine that gets this branch gets the
full feature set, including the file-search popup, git pull / push / AI commit, the search and replace
popups, and conflict routing.

## Gotchas (verified)

- **Fresh replaces symlinked `config.json`** when its settings UI saves, turning
  the dotfiles symlink into a plain copy; later dotfiles edits then silently do
  nothing. Check `ls -la ~/.config/fresh`, merge the copy's changes into
  `dotfiles/config/fresh/config.json`, rerun the installer (it backs up and re-links). New Fresh plugins
  also need their own symlink: rerun the installer. It recurs every time the settings screen saves
  (seen again: `smooth_scroll`, `highlight_current_column` and `rainbow_indentation` lived only in the
  copy), so edit settings in `dotfiles/config/fresh/config.json` instead.
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
- **Ctrl+Alt+H is Alt+Backspace** in terminals without the kitty protocol, so replace is on Ctrl+Alt+E.
- **herdr popup panes belong to no tab**: a process started from a popup has no usable `HERDR_TAB_ID` /
  `HERDR_WORKSPACE_ID`, so it must be handed the caller pane's ids. Ctrl+Q once failed with "no current
  herdr tab" until it was.
- **`npm install -g` needs `--prefix ~/.local`**: the global prefix is root-owned.
- **Running sidebars keep the old binary** until redeployed
  (`herdr plugin action invoke herdr-sidebar.redeploy`).
- Agent shells set `NO_COLOR=1`; a herdr server started from one renders
  monochrome. Start herdr from a normal shell.

## Troubleshooting

| Symptom | Check |
|---|---|
| Preview still splits a new pane in the IDE tab | Restart Fresh with `fresh-restart --all` (a Fresh pane started before the host loop has no host). `herdr pane list` should show `hs-preview-host` in the Fresh pane's tokens |
| A Fresh key does nothing | `fresh --cmd config show \| grep selection_` (config linked?), `ls -la ~/.config/fresh/plugins`, restart the daemon |
| `ctrl+shift+a` does nothing | Windows Terminal's Select-all binding; `herdr plugin action invoke herdr-sidebar.git-commit-ai` to test without the key |
| "OpenCommit is not configured" toast | run `oco setup` |
| Agent pane title stays "Agent" | the pane's shell must be zsh with the hook sourced (`. …/shell/agent-title.zsh`, or rebuild with `ctrl+b i`); check the title with `herdr pane list \| jq '.result.panes[] \| select(.label=="Agent") \| .title'` |
| "No Agent pane in this tab" | the tab has no pane labelled `Agent`; run `ctrl+b i` |
| Sidebar lacks a new feature | `herdr plugin list --json` → `plugin_root` must be this checkout; rebuild + redeploy |
| Actions missing in `herdr plugin action list` | manifest error: `herdr plugin log list --plugin herdr-sidebar` |
| Conflict colors/keys missing | the file must contain `<<<<<<<` markers; `ls -la ~/.config/fresh/config.json` must be a symlink; `fresh --cmd config show \| grep conflict_`; restart Fresh with `fresh-restart` |
| `ctrl+alt+f` / `ctrl+alt+e` does nothing | `herdr plugin action list \| grep -E 'search-popup\|replace-popup'`; `herdr server reload-config` |
| A picture opens as garbage in Fresh | pictures open in the sidebar preview since the media-routing change; rebuild and redeploy the sidebar (`dotfiles/herdr-sidebar/build.sh`) if an old build is still running. The explicit "Open in Editor" menu item still sends it to Fresh |
| A file opens in another workspace's Fresh | fixed: `fresh-open` only uses this workspace's editor; check the Fresh pane's `fresh_daemon` token with `herdr pane list` |
| `ctrl+shift+a` message never appears in the box | the tab needs a sidebar (its Source Control view takes the request); the request expires after 30 s; `oco setup` |
| `ctrl+b alt+g` (lazygit) does nothing | `command -v lazygit` (it is in `dotfiles/packages.txt`: rerun the installer without `--no-packages`, or `sudo apt install lazygit`) |
| Ctrl+click / Send to Agent / sidebar click still run scripts from `~/code/dotfiles` | rerun `docs/install-herdr-ide.sh` (it re-points the links, the `dotfiles-ide` registration and `editor-command.txt`), then `dotfiles/herdr-plugins/ide/bin/fresh-restart --all` so the daemons pick up `DOTFILES_IDE_BIN` |

More internals (herdr behaviour, heartbeat liveness, preview tabs, release
flow) are in the repo's `AGENTS.md`.
