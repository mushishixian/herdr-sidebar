#!/usr/bin/env bash
# install.sh — rebuild the herdr + Fresh terminal IDE inside WSL (Ubuntu).
#
# Lives in the herdr-sidebar repo under dotfiles/. Normally run by
# docs/install-herdr-ide.sh (which adds OpenCommit and a verify report).
#
#   ./install.sh               packages, herdr, Fresh, configs, plugins, language servers
#   ./install.sh --no-lsp      skip the language servers (dotfiles/install-lsp.sh)
#   ./install.sh --no-packages skip apt (configs, plugins and language servers' apt step)
#   ./install.sh --no-claude   don't install herdr's Claude Code hook
#   ./install.sh --lsp         accepted for older scripts; a no-op (LSP is the default)
#   ./install.sh --dry-run     print the plan and change nothing; dotfiles/verify.sh
#                              builds its checklist from those PLAN lines
#   ./install.sh --no-zshrc    leave ~/.zshrc and ~/.zshrc.local completely alone
#                              (also: dotfiles/packages.lock pins the `uv tool`
#                              installs; nothing is pinned there right now)
#                              (otherwise ~/.zshrc is replaced like any other
#                              managed config; ~/.zshrc.local is never touched)
#
# Idempotent: rerun after pulling. Any file it would replace is moved to
# ~/.dotfiles-backup/<timestamp>/ first; symlinks it made earlier are reused.
set -euo pipefail

DOTFILES=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
BACKUP="$HOME/.dotfiles-backup/$(date +%Y%m%d-%H%M%S)"
BIN="$HOME/.local/bin"
export PATH="$BIN:$PATH"

with_packages=1 with_lsp=1 with_claude=1 with_zshrc=1 dry_run=0
for arg in "$@"; do
  case $arg in
    --no-packages) with_packages=0 ;;
    --no-lsp) with_lsp=0 ;;
    --lsp) ;;
    --no-claude) with_claude=0 ;;
    --no-zshrc) with_zshrc=0 ;;
    --dry-run) dry_run=1 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }

# dry / would / plan — `--dry-run` must touch NOTHING. Every mutation below sits
# behind `dry`, and every artifact this script is responsible for is announced
# with `plan`, whose output is exactly what dotfiles/verify.sh checks. So a new
# link, tool or plugin belongs HERE (and in the matching install step) — there
# is deliberately no second manifest that could drift out of sync.
dry() { [ "$dry_run" = 1 ]; }
would() { printf '    would %s\n' "$*"; }
plan() {
  [ "$dry_run" = 1 ] || return 0
  local kind=$1 field
  shift
  printf 'PLAN\t%s' "$kind"
  for field in "$@"; do printf '\t%s' "$field"; done
  printf '\n'
}
# apt_has PKG — true when this release's apt has an install candidate for PKG.
apt_has() {
  local cand
  cand=$(apt-cache policy "$1" 2>/dev/null | sed -n 's/^  Candidate: //p') || true
  [ -n "$cand" ] && [ "$cand" != "(none)" ]
}

# link SRC DEST — symlink, backing up whatever was there.
link() {
  local src=$DOTFILES/$1 dest=$2
  plan LINK "$dest" "$src"
  if dry; then
    [ -L "$dest" ] && [ "$(readlink "$dest")" = "$src" ] && return
    would "link $dest -> $1"
    return
  fi
  mkdir -p "$(dirname "$dest")"
  if [ -L "$dest" ] && [ "$(readlink "$dest")" = "$src" ]; then return; fi
  if [ -e "$dest" ] || [ -L "$dest" ]; then
    mkdir -p "$BACKUP"
    mv "$dest" "$BACKUP/$(basename "$dest")"
    info "backed up $dest"
  fi
  ln -s "$src" "$dest"
  info "linked $dest"
}

# uv_tools — install the Python tools pinned in dotfiles/packages.lock.
#
# `uv tool install` needs no virtualenv and brings its own interpreter, so this
# is the whole Python story for the workbench. packages.lock is the only source
# of versions, and it also carries `--with` requirements: a package that provides
# NO executables (an adapter or plugin for another tool) cannot be its own tool —
# it has to go into that tool's environment. A machine whose tool already reports
# the pinned version, extras and requirements is left alone.
uv_tools() {
  local lock=$DOTFILES/packages.lock kind name version extras spec list line have
  local withs=() args=() w match
  if [ ! -f "$lock" ]; then
    info "no dotfiles/packages.lock — nothing pinned"
    return 0
  fi
  # One listing carries everything the skip test needs (version, extras, --with).
  list=$(uv tool list --show-extras --show-with 2>/dev/null || true)
  while IFS=$'\t' read -r kind name version extras; do
    [ "$kind" = uv ] || continue
    if [ -z "$name" ] || [ -z "$version" ]; then continue; fi
    spec=$name
    if [ -n "${extras:-}" ]; then spec="$name[$extras]"; fi
    spec="$spec==$version"
    withs=()
    while IFS=$'\t' read -r kind name2 want2 _; do
      if [ "$kind" = uv-with ] && [ "$name2" = "$name" ] && [ -n "$want2" ]; then
        withs+=("$want2")
      fi
    done < <(grep -v '^[[:space:]]*#' "$lock" | grep -v '^[[:space:]]*$')
    plan UVTOOL "$name" "$version"
    for w in ${withs[@]+"${withs[@]}"}; do plan UVWITH "$name" "$w"; done

    line=$(printf '%s\n' "$list" | awk -v n="$name" 'index($0, n" v") == 1 { print; exit }')
    have=$(printf '%s\n' "$line" | awk '{ sub(/^v/, "", $2); print $2 }')
    match=1
    [ "$have" = "$version" ] || match=0
    if [ -n "${extras:-}" ] && ! printf '%s\n' "$line" | grep -qF "[extras: $extras]"; then
      match=0
    fi
    for w in ${withs[@]+"${withs[@]}"}; do
      printf '%s\n' "$line" | grep -qF "$w" || match=0
    done
    if [ "$match" = 1 ]; then
      info "uv: $name $version (pinned)"
      continue
    fi

    args=(tool install --force "$spec")
    for w in ${withs[@]+"${withs[@]}"}; do args+=(--with "$w"); done
    if dry; then
      would "uv ${args[*]}"
      continue
    fi
    if ! command -v uv >/dev/null 2>&1; then
      info "uv is missing — skipped $name (install uv, then rerun)"
      continue
    fi
    if uv "${args[@]}" >/dev/null 2>&1; then
      info "uv: $name $version installed"
    else
      info "uv ${args[*]} failed — run it by hand to see why"
    fi
  done < <(grep -v '^[[:space:]]*#' "$lock" | grep -v '^[[:space:]]*$')
}


# codebuddy_hooks SETTINGS HOOK — merge this repo's hook entries into CodeBuddy's
# user settings, leaving every other key and hook alone.
#
# herdr has no CodeBuddy integration (`herdr integration install codebuddy` does
# not exist; 0.9.5's target list stops at its own agents) and never detects the
# `codebuddy` process, so the pane's agent state can only come from CodeBuddy's
# own hook system, which is Claude-Code compatible: `hooks.<Event>[]` entries
# whose `hooks[].command` runs our script. Entries are matched BY THAT COMMAND,
# so a rerun replaces only ours — the user's model, permissions, plugins and own
# hooks survive. A settings file whose JSON we cannot parse is left untouched
# and reported instead of being clobbered.
codebuddy_hooks() {
  local settings=$1 hook=$2 tmp
  if [ ! -s "$settings" ]; then
    mkdir -p "$(dirname "$settings")"
    printf '{}\n' >"$settings"
  fi
  tmp=$(mktemp "$settings.XXXXXX") || return 1
  # jq notes: `ours` matches an entry by OUR command, so a rerun replaces only
  # the entries this repo installed; `strip` removes just those hook objects and
  # then drops entries left empty, which keeps any other command sharing one.
  # (No apostrophes anywhere below: this program sits inside shell single
  # quotes, and one would end the string.)
  if jq --arg cmd "$hook" '
    def ours: .type == "command" and ((.command // "") | test("herdr-agent-state\\.sh$"));
    def strip:
      map(.hooks = ((.hooks // []) | map(select(ours | not))))
      | map(select((.hooks | length) > 0));
    def entry($matcher):
      (if $matcher then { matcher: $matcher } else {} end)
      + { hooks: [{ type: "command", command: $cmd, timeout: 5 }] };
    def add($event; $matcher):
      .[$event] = (((.[$event] // []) + [entry($matcher)]));
    .hooks = ((.hooks // {})
      | with_entries(.value |= strip)
      | add("SessionStart"; "startup|resume|clear|compact")
      | add("UserPromptSubmit"; null)
      | add("PreToolUse"; null)
      | add("Stop"; null)
      | add("PermissionRequest"; null)
      | add("Notification"; "permission_prompt|agent_needs_input|elicitation_dialog")
      | add("SessionEnd"; null))
  ' "$settings" >"$tmp" 2>/dev/null; then
    :
  else
    rm -f "$tmp"
    info "could not merge the CodeBuddy hooks into $settings (left untouched)"
    return 1
  fi
  if cmp -s "$tmp" "$settings"; then
    rm -f "$tmp"
    return 0
  fi
  mkdir -p "$BACKUP"
  cp -p "$settings" "$BACKUP/settings.json.codebuddy" 2>/dev/null || true
  mv -f "$tmp" "$settings"
  info "merged the agent-state hooks into $settings (backup in $BACKUP)"
}

[ "$(id -u)" -ne 0 ] || { echo "run as your user, not root" >&2; exit 1; }
grep -qi microsoft /proc/version 2>/dev/null || echo "note: not WSL; Windows bridging (wsl-open) will be inert"

# ── 1. Packages ────────────────────────────────────────────────────────
if [ "$with_packages" = 1 ]; then
  step "apt packages"
  mapfile -t pkgs < <(grep -v '^\s*#' "$DOTFILES/packages.txt" | grep -v '^\s*$')
  # Older releases lack some packages (24.04 has no lazygit or starship): install what apt has.
  avail=() skipped=()
  for p in "${pkgs[@]}"; do
    if apt_has "$p"; then avail+=("$p"); else skipped+=("$p"); fi
  done
  if dry; then
    would "apt-get update, then install: ${avail[*]:-(nothing)}"
  else
    sudo apt-get update -qq
    if [ "${#avail[@]}" -gt 0 ]; then
      sudo apt-get install -y -qq "${avail[@]}"
    fi
  fi
  if [ "${#skipped[@]}" -gt 0 ]; then
    info "skipped (not in this Ubuntu's apt): ${skipped[*]} — see docs/herdr-ide.md"
  fi
fi
# Ubuntu renames these two; give them their upstream names.
if dry; then
  command -v fdfind >/dev/null && would "link $BIN/fd -> $(command -v fdfind)"
  command -v batcat >/dev/null && would "link $BIN/bat -> $(command -v batcat)"
else
  mkdir -p "$BIN"
  command -v fdfind >/dev/null && ln -sfn "$(command -v fdfind)" "$BIN/fd"
  command -v batcat >/dev/null && ln -sfn "$(command -v batcat)" "$BIN/bat"
fi

# ── 2. herdr + Fresh (official installers, checksum-verified, into ~/.local) ─
step "herdr and Fresh"
if ! command -v herdr >/dev/null; then
  if dry; then would "install herdr (herdr.dev/install.sh)"; else
    curl -fsSL https://herdr.dev/install.sh | sh
  fi
else
  info "herdr $(herdr --version | awk '{print $2}') present (update: herdr update)"
fi
if ! command -v fresh >/dev/null; then
  if dry; then would "install fresh (sinelaw/fresh install.sh)"; else
    curl -fsSL https://raw.githubusercontent.com/sinelaw/fresh/refs/heads/master/scripts/install.sh \
      | sh -s -- --no-desktop-integration
  fi
else
  info "fresh $(fresh --version | awk '{print $2}') present (update: fresh --cmd update)"
fi

# Tools every config in this repo expects on PATH (keymap scripts, Fresh
# plugins, zshrc). `verify.sh` asserts each of these from the plan, so a new
# dependency belongs here AND in packages.txt / the language installer — never
# only in a doc.
for tool in herdr fresh zsh git jq rg fd bat eza fzf zoxide delta lazygit starship node python3; do
  plan TOOL "$tool"
done

# ── 2b. Python tools (uv) ──────────────────────────────────────────────
step "python tools (uv)"
if ! command -v uv >/dev/null; then
  if dry; then would "install uv (astral.sh/uv/install.sh)"; else
    curl -LsSf https://astral.sh/uv/install.sh | sh
  fi
else
  info "uv $(uv --version | awk '{print $2}') present"
fi
if [ -f "$DOTFILES/packages.lock" ]; then
  plan TOOL uv
  uv_tools
fi

# ── 3. Config files ────────────────────────────────────────────────────
step "config files"
# One machine, one shell setup: like every other managed config, ~/.zshrc is
# replaced (the old file is backed up first). Machine-only bits — SDK paths,
# work aliases, a different prompt — belong in ~/.zshrc.local, which the managed
# rc sources and this installer never touches. --no-zshrc skips both.
if [ "$with_zshrc" = 1 ]; then
  if [ -e "$HOME/.zshrc" ] && [ ! -L "$HOME/.zshrc" ]; then
    if dry; then
      would "replace your ~/.zshrc (backup first); machine-only bits belong in ~/.zshrc.local"
    else
      info "replacing your ~/.zshrc (backup first); machine-only bits belong in ~/.zshrc.local"
    fi
  fi
  link zsh/zshrc "$HOME/.zshrc"
  plan OVERLAY "$HOME/.zshrc.local"
else
  info "zsh: left alone (--no-zshrc)"
fi
link config/herdr/config.toml  "$HOME/.config/herdr/config.toml"
# The keymap's helper scripts (popup commands run `bash ~/.config/herdr/scripts/…`).
# One directory symlink, so a pulled `git pull` reaches every machine's keymap.
link config/herdr/scripts      "$HOME/.config/herdr/scripts"
link config/fresh/config.json  "$HOME/.config/fresh/config.json"
for plugin in "$DOTFILES"/config/fresh/plugins/*.ts; do
  link "config/fresh/plugins/$(basename "$plugin")" "$HOME/.config/fresh/plugins/$(basename "$plugin")"
done
link config/lazygit/config.yml "$HOME/.config/lazygit/config.yml"
link config/ripgrep/config     "$HOME/.config/ripgrep/config"
link config/bat/config         "$HOME/.config/bat/config"

for script in fresh-open fresh-ide fresh-restart herdr-ide herdr-ide-term herdr-rpc; do
  link "herdr-plugins/ide/bin/$script" "$BIN/$script"
done
link bin/wsl-open "$BIN/wsl-open"
link bin/phpactor-lsp "$BIN/phpactor-lsp"
if ! command -v xdg-open >/dev/null || [ "$(readlink -f "$(command -v xdg-open)")" = "$DOTFILES/bin/wsl-open" ]; then
  link bin/wsl-open "$BIN/xdg-open"
fi

include="$DOTFILES/config/git/dotfiles.gitconfig"
plan GIT-INCLUDE "$include"
if dry; then
  git config --global --get-all include.path 2>/dev/null | grep -qxF "$include" ||
    would "git config --global --add include.path $include"
elif ! git config --global --get-all include.path 2>/dev/null | grep -qxF "$include"; then
  git config --global --add include.path "$include"
  info "git: included $include"
fi

# ── 4. herdr plugins and integrations ──────────────────────────────────
step "herdr plugins"
plan GENERATED "$HOME/.zfunc/_herdr"
if dry; then
  would "write $HOME/.zfunc/_herdr (herdr completion zsh)"
else
  mkdir -p "$HOME/.zfunc"
  herdr completion zsh > "$HOME/.zfunc/_herdr"
fi

plugins=$(herdr plugin list --json 2>/dev/null || echo '{}')

# Patched fork: clicks open in Fresh without switching herdr tabs.
# Patched fork: clicks open in Fresh without switching herdr tabs.
REPO=$(cd "$DOTFILES/.." && pwd)
plan PLUGIN herdr-sidebar "$REPO/plugins/herdr-sidebar"
plan PLUGIN dotfiles-ide "$DOTFILES/herdr-plugins/ide"
if dry; then
  would "build herdr-sidebar from this repo (dotfiles/herdr-sidebar/build.sh)"
  would "register dotfiles-ide -> $DOTFILES/herdr-plugins/ide"
else
  "$DOTFILES/herdr-sidebar/build.sh"
  info "herdr-sidebar: built from this repo (update: git pull, then rerun)"
  # build.sh may have installed rustup; later steps (install-lsp.sh) must see it first.
  if [ -d "$HOME/.cargo/bin" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
  root=$(jq -r '.result.plugins[]? | select(.plugin_id=="dotfiles-ide") | .plugin_root' <<<"$plugins" 2>/dev/null || true)
  if [ "$root" = "$DOTFILES/herdr-plugins/ide" ]; then
    info "dotfiles-ide linked"
  elif [ -n "$root" ]; then
    herdr plugin unlink dotfiles-ide >/dev/null
    herdr plugin link "$DOTFILES/herdr-plugins/ide" >/dev/null
    info "dotfiles-ide: re-linked from $root"
  else
    herdr plugin link "$DOTFILES/herdr-plugins/ide" >/dev/null
    info "dotfiles-ide linked"
  fi
fi

# herdr-sidebar defaults: VS Code colours, and a click hands the file to the
# Fresh pane in the same tab (fresh-open, run inline: no herdr tab opens).
# Choices already made in its Settings win.
state_dir="$HOME/.local/state/herdr/plugins/herdr-sidebar"
state="$state_dir/state.json"
plan STATE "$state"
plan EDITOR-COMMAND "$BIN/fresh-open {file}"
if dry; then
  would "seed $state (VS Code colours, git footer, click opens in Fresh)"
  would "write $state_dir/editor-command.txt"
else
  mkdir -p "$state_dir"
  if [ -f "$state" ]; then
    jq -s '.[0] + .[1]' "$DOTFILES/herdr-sidebar/state.json" "$state" > "$state.tmp" && mv "$state.tmp" "$state"
  else
    cp "$DOTFILES/herdr-sidebar/state.json" "$state"
  fi
  printf '%s {file}' "$BIN/fresh-open" > "$state_dir/editor-command.txt"
  info "herdr-sidebar: click opens in Fresh ($(cat "$state_dir/editor-command.txt"))"
fi

if [ "$with_claude" = 1 ] && [ -d "$HOME/.claude" ]; then
  plan INTEGRATION claude
  if dry; then
    would "herdr integration install claude"
  else
    herdr integration install claude >/dev/null && info "Claude Code hook installed (herdr integration status)"
  fi
fi

# herdr gets an agent's state from these hooks. pi has "full lifecycle hook
# authority", so herdr skips its screen heuristics for it: without the
# integration the spaces rail shows pi idle forever. Rerun after a pi upgrade.
if [ -d "$HOME/.pi" ]; then
  plan INTEGRATION pi
  if dry; then
    would "herdr integration install pi"
  else
    herdr integration install pi >/dev/null && info "pi agent-state hook installed (herdr integration status)"
  fi
fi

# CodeBuddy is NOT one of herdr's integration targets (0.9.5 stops at its own
# agents) and herdr never detects its process, so its pane state comes from
# CodeBuddy's own hooks. The script is linked like the other dotfiles and only
# OUR entries in ~/.codebuddy/settings.json are rewritten, so the user's model,
# permissions and plugins survive a rerun. It also names the agent
# (`--agent codebuddy`) — with a name reported to herdr, the spaces rail gets
# the state AND the label, even without the zsh agent-title hook.
if command -v codebuddy >/dev/null 2>&1 || [ -d "$HOME/.codebuddy" ]; then
  link config/codebuddy/hooks/herdr-agent-state.sh "$HOME/.codebuddy/hooks/herdr-agent-state.sh"
  if dry; then
    would "merge the agent-state hooks into $HOME/.codebuddy/settings.json"
  else
    codebuddy_hooks "$HOME/.codebuddy/settings.json" "$HOME/.codebuddy/hooks/herdr-agent-state.sh" &&
      info "codebuddy agent-state hook installed (idle/working/blocked in herdr)"
  fi
fi

if dry; then
  would "herdr server reload-config"
else
  herdr server reload-config >/dev/null 2>&1 && info "reloaded the running herdr server" || true
fi

# ── 5. Language servers (default; --no-lsp skips) ─────────────────────
if [ "$with_lsp" = 1 ]; then
  step "language servers"
  lsp_args=()
  [ "$with_packages" = 1 ] || lsp_args+=(--no-apt)
  if dry; then
    would "install language servers (dotfiles/install-lsp.sh ${lsp_args[*]:-})"
  else
    "$DOTFILES/install-lsp.sh" ${lsp_args[@]+"${lsp_args[@]}"} ||
      info "some language servers failed (see the summary above; rerun dotfiles/install-lsp.sh)"
  fi
fi

# ── 6. Login shell ─────────────────────────────────────────────────────
if [ "$(getent passwd "$USER" | cut -d: -f7)" != "$(command -v zsh)" ]; then
  step "login shell"
  if dry; then
    would "chsh -s $(command -v zsh)"
  else
    chsh -s "$(command -v zsh)"
  fi
fi

if dry; then
  step "dry run"
  info "nothing was changed — the lines above are the plan"
  info "check this machine for real: dotfiles/verify.sh"
  exit 0
fi

step "done"
info "open a new shell (or: exec zsh), then run: herdr"
info "changed a managed file by hand and it stopped being a symlink? dotfiles/adopt.sh <path>"
info "in a project pane: ctrl+b i -> IDE layout, ctrl+b ? -> all keys"
[ -d "$BACKUP" ] && info "replaced files are in $BACKUP"
exit 0
