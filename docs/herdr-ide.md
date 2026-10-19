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
| herdr-sidebar fork | `~/code/herdr-sidebar`, branch `main` (this repo) | Rust herdr plugin, linked with `herdr plugin link` |
| Dotfiles | `dotfiles/` in this repo (branch `main`; pushed to the fork `mushishixian/herdr-siderbar`) | configs + scripts |
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
herdr-sidebar/                 this repo (branch main)
├── plugins/herdr-sidebar/     Rust plugin: explorer, search, source control (upstream-shaped, builds standalone)
├── dotfiles/                  IDE layer: install.sh, herdr/Fresh/lazygit/shell configs, dotfiles-ide plugin, build.sh
└── docs/                      this document and install-herdr-ide.sh
```

`plugins/herdr-sidebar` never references `dotfiles/`, so upstream merges stay clean.

## Install

**Requirements.** Windows with WSL and an Ubuntu distro, plus Windows Terminal (or WezTerm) with a
Nerd Font selected as the terminal font, so the sidebar and Fresh icons render. Ubuntu 25.10 or newer
(26.04 LTS recommended) gets every package from apt. On Ubuntu 24.04 the installer still works but
skips `lazygit` and `starship` (apt does not have them there): `starship` is only the optional shell
prompt, and `lazygit` is the `ctrl+b alt+g` popup. The sidebar is built with a rustup toolchain,
because 24.04's apt `rustc` is too old for the crate (`rust-version` 1.89).

Clone `main` of the fork, `https://github.com/mushishixian/herdr-siderbar.git` (the GitHub
repo is spelled `siderbar`; the checkout directory is still `~/code/herdr-sidebar`). HTTPS works
without an SSH key because the repo is public. The upstream `alexarthurs/herdr-sidebar` does NOT
contain these commits. The dotfiles live in the fork's `dotfiles/` directory, so there is no second
repo to fetch.

```bash
git clone https://github.com/mushishixian/herdr-siderbar.git ~/code/herdr-sidebar
cd ~/code/herdr-sidebar
docs/install-herdr-ide.sh            # --help for options (--no-lsp, --no-packages, --no-claude, --no-oco)
```

Language servers are installed by default (`dotfiles/install-lsp.sh`, see
*Language servers (LSP)*). `--no-lsp` skips them; `--no-packages` also skips their apt step.

What it does, in order:

1. Drift check: warns if Fresh or herdr replaced their config symlink with a plain
   copy (see *Gotchas*) and shows the diff so you can merge it into `dotfiles/config/` first.
2. Runs `dotfiles/install.sh`: apt packages (incl. `rustc cargo build-essential jq`; any package this
   Ubuntu's apt does not have is skipped with a note, never aborting the install), herdr + Fresh
   official installers, all symlinks, `dotfiles/herdr-sidebar/build.sh` (builds
   `plugins/herdr-sidebar` from this checkout and points herdr's `herdr-sidebar`
   registration at it, replacing any other registration), links `dotfiles-ide`,
   seeds the sidebar settings, installs herdr's Claude Code and pi agent-state hooks.
   `build.sh` uses the first Rust toolchain at least as new as the crate's `rust-version`: the
   `rustc` on PATH, else an existing `~/.cargo/bin` (rustup), else it installs rustup into your
   home directory, non-interactively.
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
| `ctrl+alt+n` | Git: new branch — name prompt (popup), branched from HEAD and switched to |
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
| `alt+shift+q` | LSP: go to the definition under the cursor (same as `F12`) |
| `alt+shift+w` | LSP: find references to the symbol under the cursor (same as `Shift+F12`) |
| `alt+o` | LSP: list this file's class/method/function symbols and jump (`lsp_navigation`) |

The LSP trio is `alt+shift+q` / `alt+shift+w` / `alt+o` rather than `alt+q` /
`alt+w`: herdr binds `alt+w` to the lazygit popup and takes keys before the pane's
program does, so any `alt+w` binding inside Fresh is unreachable. Fresh's own
`F12` / `Shift+F12` / `Ctrl+F12` (definition / references / implementation) are
untouched and still work. `Ctrl+Shift+<letter>` is not usable either: without the
kitty keyboard protocol the terminal sends the same bytes as plain
`Ctrl+<letter>` — `Ctrl+Shift+O` arrives as `Ctrl+O` and opens the file picker.
Bare modifiers cannot be bound at all, so "double-tap Shift" is not expressible;
`alt+o` is the document-symbol list instead. `goto_lsp_symbol` is a plugin action
from the bundled `lsp_navigation` plugin: bind it by name in `keybindings`, and
read *Gotchas* before wrapping it.

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

Anything that asks you to type (new file/folder, rename, typed change folder,
custom editor, pane rename) opens as a small bordered dialog over the tree;
`⏎` accepts, `esc` cancels. Deleting from the Explorer asks first — turn that
off with the ⚙ Settings row **Confirm delete** (`on`/`off`, remembered across
sessions) when you would rather have no prompt at all.

## How each piece works

### Which space's agent is busy (herdr's spaces rail)

herdr aggregates every pane's agent state into a per-workspace `agent_status`
(`workspace list`), and the top-left rail draws it: row 1's `state_icon` is the
workspace's status (symbols style: `◐` working, `×` blocked, `✓` done, `○` idle,
`·` unknown — colours yellow / red / teal / green / grey). That glyph is a single
cell, so `dotfiles/config/herdr/config.toml` keeps **two lines per space, no blank
line between spaces** (`row_gap = 0`): row 1 is the bold project name with the
space's agent names beside it, row 2 is the branch/git line. The busy/idle *word*
is not drawn — the icon and its colour carry it; adding `state_text` back is one
config line (`rows` is a plain list).

**That block only reaches the rail if `~/.config/herdr/config.toml` is the installer's
symlink.** Replace that file with your own config (a personal keymap file is the common
case) and herdr silently falls back to its defaults: `status_indicators = "dots"`, where
working / blocked / done are all a `●` and only the colour differs, plus two rows
(`["state_icon", "workspace"]`, `["branch", "git_status"]`) with no word line. The status
data is still correct — `herdr workspace list` reports `agent_status = working` — it is
just unreadable. Merge `[ui] status_indicators = "symbols"` and the
`[ui.sidebar.spaces] rows` table into your file, then `herdr server reload-config`
(applies to a LIVE client, no restart).

**The agent list under the spaces cannot be hidden.** `[ui.sidebar.agents]` accepts only
`row_gap`, `rows` and `rows_by_agent` — there is no enable/visibility switch — and the
`agents … grouped` header is drawn unconditionally. `rows = []` does NOT remove the section: it
degrades to **one line per agent holding just the state dot** (verified live: 9 agent panes →
9 bare `○` lines), i.e. the same height as the one-line form but with no information. The floor
is one row per detected agent pane, so the section is as tall as your agent count. Give those
rows content (`rows = [["state_icon", "agent"]]`) instead of leaving them empty, and reclaim the
space by collapsing the rail (`prefix+b`; `sidebar_collapsed_mode = "compact"` keeps a narrow
status strip, `"hidden"` uses zero width, `sidebar_start_collapsed = true` starts collapsed).

The agents are named beside the project name, because a bare `◐` does not say
*what* is running. herdr has no built-in "agent name" token, but it renders any
`$custom` token in those rows, so the sidebar reports one per space: `$agents`
reads `pi` for a single-agent space and
`pi · codex` for one running two agents, busy ones first. `plugins/herdr-sidebar`
sends it every 5 s from each sidebar pane's heartbeat (`src/agent_tokens.rs`,
source `herdr-sidebar`, `ttl_ms` 20 s), so it expires on its own when the last
sidebar of a session goes away, and it clears a space that has no agent pane.
Agents are read from `pane.list`, so this needs no extra configuration. Toggling
herdr's own sidebar (`prefix+b`, "agent sidebar") shows or hides the whole rail,
these per-space agent lines included.

Two things that surprise people:

- **The aggregation is a max, and working is not the top of it**: blocked > done
  (finished, not looked at yet) > working > idle > unknown. A space whose agent
  is working shows `◐` only while nothing louder is in it.
- **The state has to be reported.** herdr gets it from per-agent hooks
  (`herdr integration install <agent>`; the installer installs `pi` and Claude
  Code). `pi`, `omp`, `mastracode`, `opencode`, `kilo` and `kimi` have *full hook
  authority*, so herdr does not scan their screens at all: without the hook the
  rail shows them idle forever, and a **hung** hook agent can stay `◐` until the
  process exits (there is no staleness timeout). Other agents are detected from
  the screen tail and go back to idle within about a second.
- **Background subagents used to make the space look idle.** pi's own hook reports
  `working` for the main agent's turn and `idle` the moment that turn settles, and
  `@tintinweb/pi-subagents` (the setup here) launches agents in the **background by
  default**: the dispatch returns immediately, so the space flipped to `○` while the
  child agent kept working (measured: 48 s of `idle` during one background run).
  Foreground (blocking) subagents were never affected — the turn stays open while they
  run. The sidebar fixes this itself (`plugins/herdr-sidebar/src/pi_state.rs`, no extra
  pi extension): it reads pi's persisted transcript for every pi pane in the session
  and keeps the pane `◐` while a background agent is unfinished or a turn is open, so the
  space stops looking idle. Expect the rail to catch up within ~5 s of a dispatch or a
  completion (it rides the sidebar's heartbeat). A background agent only counts while its
  own output file (`/tmp/pi-subagents-<uid>/…/<id>.output`) changed in the last 5 minutes:
  pi `/reload` or quitting pi stops its agents without recording it, and without this rule
  the space stayed `◐` forever. The cost is that an agent stuck in one tool call for more
  than 5 minutes shows `○` until it writes again. Two caveats: the sidebar has to report
  the pane's state itself (herdr only accepts it from pi's own identity), so pi panes
  show the old behaviour while **no sidebar** runs anywhere in the session, and quitting
  every sidebar freezes their indicator until pi restarts. Subagent
  activity inside a *nested* child (a workflow's agents) is covered by the top-level
  agent's run, which is what the transcript records.

### IDE layout: `dotfiles-ide` plugin

- **Pane identity is by label**, so it survives herdr restarts: `Fresh`,
  `Terminal`, `Agent`; `Sidebar` belongs to herdr-sidebar. The Fresh pane also
  carries a `fresh_daemon=<name>` metadata token.
- **But a label alone does not mean the editor is up.** A herdr server restart
  (or a reboot) resumes every pane as a plain SHELL: the labels come back, the
  metadata tokens do not, and no Fresh daemon runs. Such a pane is a *corpse*.
  `lib.sh`'s `fresh_editor_ok` therefore asks for the `fresh_daemon` token AND a
  live daemon. `herdr-ide` treats a tokenless `Fresh` pane as an editor to heal
  IN PLACE — same pane, relabelled and started again with `fresh-ide`, no extra
  splits — and prints the editor pane id on every path, which is what
  `fresh-open` reads. Regression symptom (before this): the first file click
  after a restart reported `custom editor failed: …/fresh-open exited with exit
  status: 1` (its `die "could not start the IDE editor"`) while the pane showed
  an empty prompt.
- `bin/herdr-ide` (action `layout`) splits the current tab into the layout
  above. Started from an agent pane, that agent becomes the right column; from
  a shell, the shell becomes the Terminal and a new agent opens (agent command:
  plugin config `agent` file, else `$IDE_AGENT`, else the first INSTALLED agent —
  **pi before claude**, since claude is not a given; `none` = no column, which is
  also what you get when neither is installed). An explicit choice always wins.
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
  column is left to herdr-sidebar. A sidebar re-dock no longer resets these: the
  sidebar restores a tab's split ratios around its own full-height repair, and
  `ide-save-sizes` stays quiet for ~20 s afterwards (its `dock/restore.stamp`)
  so the intermediate ratios are never recorded.
- **Terminal height** (the `Terminal` "command line" under the editor) has a default in the
  sidebar's ⚙ Settings: the row `Terminal height`, one below `Sidebar width`. It is measured in
  ROWS (4–40, `←`/`→` in 2-row steps; `⏎` resets it to `auto`). Only the height is configurable —
  the width is whatever the middle column is. `←`/`→` also resizes the terminal in the tab the
  sidebar is in, right away; `⏎` does not touch the current layout. When a layout is BUILT
  (`herdr-ide`, `fresh-restart`'s rebuilt editor, `herdr-ide-term` reopening a closed terminal)
  the order is: the project's remembered ratio in `sizes.json` (your own drag) wins, then this
  setting, then the historical 70% editor share. `lib.sh`'s `editor_split_ratio` computes it as
  `1 - rows / column height`, clamped to herdr's 0.1..0.9, and reads the setting from
  `~/.local/state/herdr/plugins/herdr-sidebar/state.json` (`terminal_height`); it never writes
  there.
- **Agent width** (the rightmost `Agent` column) has the same kind of default: the ⚙ Settings row
  `Agent width`, one below `Terminal height`. It is measured in COLUMNS (24–320, `←`/`→` in 4-column
  steps; `⏎` resets it to `auto`). Only the width is configurable — the height is whatever the tab
  is. `←`/`→` also resizes the Agent column in the tab the sidebar is in, right away; `⏎` does not
  touch the current layout. Build order (`herdr-ide`'s two sites) is: `sizes.json`'s remembered
  `agent` ratio (your own drag) wins, then this setting, then the historical 0.6 middle / 0.4 agent
  share. `lib.sh`'s `agent_split_ratio` computes `1 - cols / column width` (the split ratio is the
  middle column's share), clamped to herdr's 0.1..0.9, and reads `agent_width` from the same sidebar
  state file; it never writes there.
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

Upstream: `alexarthurs/herdr-sidebar`, forked at `v0.15.1`. Commits on `main` (the fork):

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
| *uncommitted* | **"Git init" footer button** | When the root has no repository above it, the Explorer footer shows a clickable `Git init` chip (`Git::is_initialized`, a `.git` walk with no git process, refreshed on tick). It runs `git init` on a worker, then rediscovers repos/decorations: branch strip and `U` markers appear immediately. Mouse-only, the zone is reset every frame. |
| *uncommitted* | **Search results open in the editor** | The Search view and Quick Open forced the built-in preview (`open_preview_at`) even with a custom editor configured; they now go through `explorer_app::open_file` → `file_popup::open_file_in_workspace`, i.e. the same routing a tree click uses (Fresh at the hit's line, else the preview). Media goes to the editor too (the media exception was dropped later), and the preview path keeps the double-click pin bookkeeping. |
| *uncommitted* | **Merge popup merges on one key** | The `--git-popup merge` branch list used to ask "Merge X into Y?" on a second screen (`Screen::ConfirmMerge`); picking a branch now merges immediately, and the result/conflict message screen (with `a` for `merge --abort`) is unchanged. `Git::merge_branch` still passes `--no-edit` and never forces. |
| *uncommitted* | **Commit button offers a pull; both oco token ceilings pinned** | When the branch is behind its upstream AND something is staged, ✓ Commit reads `✓ Commit · N↓` and asks "Pull first?" (`y` = plain `git pull --no-rebase`, then commit; anything else commits at once); a failed pull never commits. The AI-commit path never asks. `Git::pull` must pass `--no-rebase` because this machine's `dotfiles/config/git/dotfiles.gitconfig` sets `pull.rebase=true`, which refuses to run with a staged index. `OCO_TOKENS_MAX_OUTPUT=500` is now pinned next to `OCO_TOKENS_MAX_INPUT=24000`, since oco subtracts it from the input budget. |
| *uncommitted* | **New branch popup** | `--git-popup[-open] new` (`Op::New`, `ctrl+alt+n`): an input-only popup that creates the branch from HEAD and switches to it (`git switch -c`, name judged by `git check-ref-format --branch`, no stash/force), with the error in the footer instead of a toast. Manifest pane `git-new-branch`, action `git-new-branch`. |
| *uncommitted* | **Instant branch reaction** | `Git::head_signature` (the git dir's `HEAD`; resolves a worktree's `.git` FILE) is compared a few times a second by both views, so a checkout / create / rename from a popup or a terminal refreshes the SCM header and the Explorer's footer/decorations at once instead of waiting for the 1.5 s status poll or `DECO_REFRESH`. Measured: ~65 ms (SCM), ~0.5 s (Explorer), both were 2-3 s. |
| *uncommitted* | **Inline editor failures are reported** | A click only reports the SPAWN, and the editor runs with null stdio, so a command that died on the spot looked like a successful open. `spawn_inline_editor` now reaps the child on a thread and `actions::take_editor_failures` hands a non-zero exit within 30 s to Explorer's notice / Source Control's flash (`custom editor failed: <program> exited with <status>`). |
| *uncommitted* | **IDE layer: a label-only Fresh pane is healed in place** | A herdr server restart resumes panes as plain shells, keeping labels but dropping metadata tokens, so `herdr-ide`'s label-only check treated a dead `Fresh` pane as a working editor: it focused it, printed nothing, and `fresh-open` died with "could not start the IDE editor" (exit 1, surfaced by the sidebar as `custom editor failed: …/fresh-open exited with exit status: 1`). `lib.sh` adds `pane_fresh_daemon` and `fresh_editor_ok` (token AND live daemon); `herdr-ide` reuses a corpse pane as the editor (relabel + `fresh-ide` in place, no new splits) and prints the editor pane id on the already-built path, which is how `fresh-open` gets it. |
| *uncommitted* | **IDE layer: jq ≥ 1.6 keyword fix** | `lib.sh`, `ide-save-sizes` and `fresh-to-agent` named a jq variable `label`, which jq 1.6+ parses as its `label` keyword (`$label` → "syntax error, unexpected label"), so EVERY pane lookup failed on Linux/WSL and a file click built no editor. Variables are `$lbl` now; `fresh-open` also reports a failed `herdr-ide` instead of exiting 3 silently. |
| *uncommitted* | **Media opens in the preview; previews cover Fresh; Agent pane title** | `viewer::is_media_file` keeps pictures and videos out of the custom editor (Explorer click, Ctrl+Q popup; Source Control's `diff_opens_in_editor` media flag). Preview host: `fresh-ide` runs a host loop in the Fresh pane (token `hs-preview-host`); the sidebar writes `<sidebar state dir>/preview-host/<pane>.req`, the host claims it by rename, and the viewer runs there with `HERDR_SIDEBAR_PREVIEW_HOSTED=1`; `viewer::uncover_host` runs before the editor launches. Agent pane title: `ide-agent-title` + `shell/agent-title.zsh` (zsh preexec/precmd) → `pane report-metadata --title "◆ CLAUDE"`; language servers: `dotfiles/install-lsp.sh` (default in the installer) + Fresh `lsp` auto_start. |

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

Big or generated files are shortened for the AI only: the first 40 lines of a new file, or the
first hunks of a changed one, and generated/vendored paths (`*.min.js`, `*.map`, `dist/`, …) or
anything over about 12 KB. The commit still contains every change. The sidebar also sends oco BOTH
token ceilings whatever `~/.opencommit` says — `OCO_TOKENS_MAX_INPUT=24000` so oco never splits
the diff into several messages, and `OCO_TOKENS_MAX_OUTPUT=500` (oco's own default) because oco
subtracts the output ceiling from that input budget.
When a file was shortened the flash says how many ("N large file(s)
summarised for the AI").

While it runs, the ✓ Commit button shows a braille spinner with "AI generating…" (then "Committing…")
and a sweeping highlight band. Clicking ✓ Commit during that just says "AI commit in progress…".

Sidebar defaults seeded by the installer (`dotfiles/herdr-sidebar/state.json`):
VS Code colours, git footer, auto-open, follow the neighbour pane's folder,
preview in a pane (a git-change diff opens beside the sidebar inside the IDE tab, so the Agent column stays visible; Esc/q closes it), editor on click, editor inline.
An inline editor launch is fire-and-forget (`fresh-open` runs with null stdio), so the sidebar
reports the editor's own failure in its footer: a command that exits non-zero within 30 s of the
click shows `custom editor failed: <program> exited with <status>` in Explorer and Source Control.
Without that, a broken editor command — say `fresh-open` unable to find or start Fresh — looked
exactly like a successful open.
`Terminal height` is deliberately NOT seeded: unset ("auto") keeps the IDE layer's own
precedence (remembered project ratio, else 70%). `Agent width` is not seeded either (else the
historical 0.6, i.e. a 40% Agent column).

Every open goes to Fresh once an editor is configured — pictures and videos included (the old
"media always opens in the terminal preview" rule is gone; it made the same file open in different
places depending on the gesture). The preview still DRAWS media whenever something sends it there:
no editor configured, the diff viewer, `Open Diff Preview`, or the preview pane opened directly.
Pictures: png, jpg/jpeg, gif, webp, bmp, ico, tif/tiff. Videos (avi, flv, m4v, mkv, mov, mp4, mpeg,
mpg, webm, wmv) show their first frame and need `ffmpeg`. For the full-resolution file, use `m` →
"Open with Default App" (runs `xdg-open`; on WSL, `dotfiles/bin/wsl-open <file>` hands it to Windows).
A changed picture in Source Control opens in Fresh too (no media special case). The explicit "Open in Editor"
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
- Enter opens the match at its line in THIS workspace's Fresh (`fresh-open` with `file:line`),
  and the sidebar's own Search view / Quick Open now do the same (`explorer_app::open_file` →
  `file_popup::open_file_in_workspace`, the routing a tree click uses) instead of always
  forcing the built-in preview.
- Replace safety: Alt+Enter (replace all) and Ctrl+R on a file header ask `y/N` first. A single replace
  re-verifies that its line still matches at that byte range. Writes are atomic (temp file, rename), keep
  permissions, CRLF and BOM, and never leave the project root (symlinks are canonicalized).
- The sidebar's own Search view still has an inert Replace field; replacement lives only in the popup.
- Popup panes belong to no tab, so both popups (and Ctrl+Q, since `aedfbd6`) hand the editor
  the caller pane's `HERDR_PANE_ID` / `HERDR_TAB_ID` / `HERDR_WORKSPACE_ID`.

### Language servers (LSP)

Fresh starts a language server for a file only when its `lsp` entry in
`dotfiles/config/fresh/config.json` sets `auto_start` (the entry is merged over Fresh's
built-in command, so the command itself is not repeated there).
`dotfiles/install-lsp.sh` installs the servers; the installer runs it by default.

| Language | Server (command) | Installed by / location |
|---|---|---|
| Markdown | `marksman` | GitHub release binary → `~/.local/bin/marksman` |
| Go | `gopls` | `go install` (apt `golang-go`) → `~/.local/bin/gopls` |
| Rust | `rust-analyzer` | `rustup component add rust-analyzer rust-src` when rustup exists, else the release binary → `~/.local/bin/rust-analyzer`; apt `rust-src` gives std completions. Releases since 2026-07-27 refuse toolchains older than 1.94, so with an older distro `rustc` (apt's is 1.93) the script pins release `2026-07-20` |
| Dart | `dart language-server --protocol=lsp` | checksum-verified SDK zip → `~/.local/share/dart-sdk`, linked as `~/.local/bin/dart`. A `dart` already on PATH (e.g. Flutter's) is left alone |
| PHP | `phpactor language-server` | phar → `~/.local/bin/phpactor`; needs apt `php-cli php-xml php-mbstring` |
| JavaScript / TypeScript | `typescript-language-server --stdio` | npm into `~/.local` (with `typescript`; also the Bash, YAML and JSON servers) |
| Python | `pylsp` | apt `python3-pylsp`; installed, but no `lsp.python` entry, so it does not auto-start |

- **Update**: `dotfiles/install-lsp.sh --update` reinstalls and upgrades everything it manages.
  Without flags it installs only what is missing. `--only markdown,go` limits the languages.
- **Add a language**: (1) add an install step and a row in the `servers` table of
  `dotfiles/install-lsp.sh`, plus its apt packages in the decision block; (2) add
  `"<language>": { "auto_start": true }` under `lsp` in `dotfiles/config/fresh/config.json`;
  (3) add its command to the verify list in `docs/install-herdr-ide.sh`; (4) add a row to this table.
- **Workspace trust**: Fresh starts language servers only in a *Trusted* workspace (status bar
  `Trusted`, stored in `~/.local/share/fresh/workspaces/<path>/trust.json`). A *Restricted* one shows
  `LSP (off)`; trust it with the command palette (`Ctrl+P`, "Trust This Folder" / "Workspace Trust") or the trust dialog's "Trust folder & Allow Tooling (T)".
- **Logs**: `~/.local/state/fresh/logs/lsp/` (one log per language and Fresh process).
- **Apply config changes**: restart the Fresh daemons with `fresh-restart --all`. Fresh reads
  `lsp` only at start.

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

# pull main (or rerun docs/install-herdr-ide.sh), then rebuild and re-point herdr at this checkout
git pull
dotfiles/herdr-sidebar/build.sh

# take upstream herdr-sidebar changes (origin is the fork, mushishixian/herdr-siderbar; upstream is alexarthurs/herdr-sidebar), then rebuild the same way
git remote add upstream https://github.com/alexarthurs/herdr-sidebar.git   # once
git fetch upstream && git merge upstream/main
dotfiles/herdr-sidebar/build.sh
```

`fresh-restart` with no argument restarts only this tab's Fresh (it is also the dotfiles-ide action
`restart-fresh`).

There is no patch set to maintain: the fork and the dotfiles are one repo, so an upstream update is a
normal git merge and `build.sh` builds whatever is checked out. A machine that gets this branch gets the
full feature set, including the file-search popup, git pull / push / AI commit, the search and replace
popups, and conflict routing.

## Gotchas (verified)

- **Never wrap a Fresh command in a plugin that calls it by its own registered
  name.** `plugins/goto_symbol.ts` (removed) bound `alt+o` to an action whose
  handler did `editor.runCommand("Go to LSP Symbol")` — the name that same plugin
  had registered, so `runCommand` → handler → `runCommand`, forever. It is async,
  so it never overflows a stack: the daemon just spins at 20 %+ CPU around the
  clock, focused or not. Signature in `~/.local/state/fresh/logs/fresh-server-<pid>.log`:
  thousands of `start_action: <handler> is a function, invoking...` lines per
  second plus `no registered command named ...` for the earlier candidates in the
  lookup list. `goto_lsp_symbol` is bindable directly; a wrapper is never needed.
- **`fresh --cmd config show` filters plugin actions out of its output.** An entry
  whose `action` is a plugin action (e.g. `goto_lsp_symbol`, `git_line_history`)
  is missing from the dump even though the binding works — do not read that as
  "the entry was dropped". Verify plugin-action bindings by pressing the key, or
  grep the config file, not `config show`.
- **Idle Fresh daemons are not free.** Each daemon carries its own plugins, LSP
  servers and timers, and herdr keeps one daemon per workspace alive even after
  its pane is replaced. Baseline is ~2-3 % CPU per daemon; two things push one
  daemon far above it: the recursion above, and orphaned daemons nobody is
  attached to (they keep polling git and re-rendering). Audit with
  `ps -eo pid,pcpu,args | grep 'fresh-editor/fresh --server'` (lifetime average;
  `%CPU` from `/proc/<pid>/stat` sampled twice gives the live number), then
  `fresh --cmd daemon list` and `fresh --cmd daemon kill <name>` for the ones no
  pane reports a `fresh_daemon` token for. `fresh-restart --all` only covers the
  `ide-*` panes herdr-ide knows about, not strays. Also worth knowing: the
  per-daemon DEBUG logs in `~/.local/state/fresh/logs/` grow ~20 MB/hour while a
  loop like that runs.
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
| After a herdr restart (or a reboot) the IDE is empty shells and a file click reports `fresh-open` exiting 1 | `herdr pane list` shows the `Fresh` pane with **no** `fresh_daemon` token: herdr restored it without metadata and nothing runs in it. Press `ctrl+b i` (or click a file again) — `herdr-ide` heals that pane in place, so no rebuild and no lost layout. `fresh-restart` deliberately skips tokenless panes |
| A Fresh key does nothing | `fresh --cmd config show \| grep selection_` (config linked?), `ls -la ~/.config/fresh/plugins`, restart the daemon |
| Fresh burns CPU while unfocused, a daemon sits at 15-25 % | `ps -eo pid,pcpu,args \| grep 'fresh-editor/fresh --server'`, then sample `/proc/<pid>/stat` twice for the live number. Two known causes: a plugin recursing into its own command (*Gotchas*), and a stray daemon with no pane attached — `fresh --cmd daemon list`, `fresh --cmd daemon kill <name>`. Baseline is ~2-3 % per editor daemon (plugins + LSP + timers). `tail -200 ~/.local/state/fresh/logs/fresh-server-<pid>.log` names the looping handler |
| An LSP key does nothing | `alt+shift+q` / `alt+shift+w` / `alt+o` need a trusted workspace and a running server (status bar `LSP (on)`; `F12` works too). If `alt+w` opened lazygit, herdr still owns it — `herdr server reload-config`, then compare `dotfiles/config/herdr/config.toml` |
| `ctrl+shift+a` does nothing | Windows Terminal's Select-all binding; `herdr plugin action invoke herdr-sidebar.git-commit-ai` to test without the key |
| "OpenCommit is not configured" toast | run `oco setup` |
| Agent pane title stays "Agent" | the pane's shell must be zsh with the hook sourced (`. …/shell/agent-title.zsh`, or rebuild with `ctrl+b i`); check the title with `herdr pane list \| jq '.result.panes[] \| select(.label=="Agent") \| .title'` |
| Spaces rail shows a space's agent idle while it is working | `herdr integration status`: the agent needs its hook (`herdr integration install pi`). An agent STARTED BEFORE the install must be **restarted** — `/reload` is not enough (verified: the hook only arms itself on `session_start`, which a reload does not re-emit) |
| "No Agent pane in this tab" | the tab has no pane labelled `Agent`; run `ctrl+b i` |
| Sidebar lacks a new feature | `herdr plugin list --json` → `plugin_root` must be this checkout; rebuild + redeploy |
| Actions missing in `herdr plugin action list` | manifest error: `herdr plugin log list --plugin herdr-sidebar` |
| Conflict colors/keys missing | the file must contain `<<<<<<<` markers; `ls -la ~/.config/fresh/config.json` must be a symlink; `fresh --cmd config show \| grep conflict_`; restart Fresh with `fresh-restart` |
| `ctrl+alt+f` / `ctrl+alt+e` does nothing | `herdr plugin action list \| grep -E 'search-popup\|replace-popup'`; `herdr server reload-config` |
| A picture opens as garbage in Fresh | pictures open in the sidebar preview since the media-routing change; rebuild and redeploy the sidebar (`dotfiles/herdr-sidebar/build.sh`) if an old build is still running. The explicit "Open in Editor" menu item still sends it to Fresh |
| A file opens in another workspace's Fresh | fixed: `fresh-open` only uses this workspace's editor; check the Fresh pane's `fresh_daemon` token with `herdr pane list` |
| AI commit is slow or writes several messages | Update the sidebar (`dotfiles/herdr-sidebar/build.sh`) so the curated diff is used; lower `OCO_TOKENS_MAX_INPUT` in `~/.opencommit` too for manual `oco` runs (the sidebar already passes 24000) |
| `ctrl+shift+a` message never appears in the box | the tab needs a sidebar (its Source Control view takes the request); the request expires after 30 s; `oco setup` |
| A `*.log` tab opens when I open a file | That language's server is missing and `lsp` auto-starts it: `dotfiles/install-lsp.sh --only <language>` (markdown, go, rust, dart, php, js, python), then `fresh-restart --all` |
| Status bar says `Restricted` / `LSP (off)` | the workspace is not trusted: Fresh never starts servers there. `Ctrl+P` → "Trust This Folder" |
| `LSP (rust)`: "outdated toolchain version" | the downloaded rust-analyzer is newer than your `rustc` supports: `dotfiles/install-lsp.sh --update --only rust` picks the release that matches |
| No completions in Rust std | `rust-src` is missing: `apt-cache policy rust-src`, then `dotfiles/install-lsp.sh --update` (or `rustup component add rust-src`) |
| `ctrl+b alt+g` (lazygit) does nothing | `command -v lazygit`. Ubuntu 25.10+: `sudo apt install lazygit`. Older Ubuntu (24.04) has no lazygit package: install a release binary from github.com/jesseduffield/lazygit/releases into `~/.local/bin` |
| Ctrl+click / Send to Agent / sidebar click still run scripts from `~/code/dotfiles` | rerun `docs/install-herdr-ide.sh` (it re-points the links, the `dotfiles-ide` registration and `editor-command.txt`), then `dotfiles/herdr-plugins/ide/bin/fresh-restart --all` so the daemons pick up `DOTFILES_IDE_BIN` |

More internals (herdr behaviour, heartbeat liveness, preview tabs, release
flow) are in the repo's `AGENTS.md`.
