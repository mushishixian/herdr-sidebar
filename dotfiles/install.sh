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
#
# Idempotent: rerun after pulling. Any file it would replace is moved to
# ~/.dotfiles-backup/<timestamp>/ first; symlinks it made earlier are reused.
set -euo pipefail

DOTFILES=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
BACKUP="$HOME/.dotfiles-backup/$(date +%Y%m%d-%H%M%S)"
BIN="$HOME/.local/bin"
export PATH="$BIN:$PATH"

with_packages=1 with_lsp=1 with_claude=1
for arg in "$@"; do
  case $arg in
    --no-packages) with_packages=0 ;;
    --no-lsp) with_lsp=0 ;;
    --lsp) ;;
    --no-claude) with_claude=0 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }

# link SRC DEST — symlink, backing up whatever was there.
link() {
  local src=$DOTFILES/$1 dest=$2
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

[ "$(id -u)" -ne 0 ] || { echo "run as your user, not root" >&2; exit 1; }
grep -qi microsoft /proc/version 2>/dev/null || echo "note: not WSL; Windows bridging (wsl-open) will be inert"

# ── 1. Packages ────────────────────────────────────────────────────────
if [ "$with_packages" = 1 ]; then
  step "apt packages"
  mapfile -t pkgs < <(grep -v '^\s*#' "$DOTFILES/packages.txt" | grep -v '^\s*$')
  sudo apt-get update -qq
  sudo apt-get install -y -qq "${pkgs[@]}"
fi
# Ubuntu renames these two; give them their upstream names.
mkdir -p "$BIN"
command -v fdfind >/dev/null && ln -sfn "$(command -v fdfind)" "$BIN/fd"
command -v batcat >/dev/null && ln -sfn "$(command -v batcat)" "$BIN/bat"

# ── 2. herdr + Fresh (official installers, checksum-verified, into ~/.local) ─
step "herdr and Fresh"
if ! command -v herdr >/dev/null; then
  curl -fsSL https://herdr.dev/install.sh | sh
else
  info "herdr $(herdr --version | awk '{print $2}') present (update: herdr update)"
fi
if ! command -v fresh >/dev/null; then
  curl -fsSL https://raw.githubusercontent.com/sinelaw/fresh/refs/heads/master/scripts/install.sh \
    | sh -s -- --no-desktop-integration
else
  info "fresh $(fresh --version | awk '{print $2}') present (update: fresh --cmd update)"
fi

# ── 3. Config files ────────────────────────────────────────────────────
step "config files"
link zsh/zshrc                 "$HOME/.zshrc"
link config/herdr/config.toml  "$HOME/.config/herdr/config.toml"
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
if ! command -v xdg-open >/dev/null || [ "$(readlink -f "$(command -v xdg-open)")" = "$DOTFILES/bin/wsl-open" ]; then
  link bin/wsl-open "$BIN/xdg-open"
fi

include="$DOTFILES/config/git/dotfiles.gitconfig"
if ! git config --global --get-all include.path 2>/dev/null | grep -qxF "$include"; then
  git config --global --add include.path "$include"
  info "git: included $include"
fi

# ── 4. herdr plugins and integrations ──────────────────────────────────
step "herdr plugins"
mkdir -p "$HOME/.zfunc"
herdr completion zsh > "$HOME/.zfunc/_herdr"

plugins=$(herdr plugin list --json 2>/dev/null || echo '{}')

# Patched fork: clicks open in Fresh without switching herdr tabs.
"$DOTFILES/herdr-sidebar/build.sh"
info "herdr-sidebar: built from this repo (update: git pull, then rerun)"
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

# herdr-sidebar defaults: VS Code colours, and a click hands the file to the
# Fresh pane in the same tab (fresh-open, run inline: no herdr tab opens).
# Choices already made in its Settings win.
state_dir="$HOME/.local/state/herdr/plugins/herdr-sidebar"
mkdir -p "$state_dir"
state="$state_dir/state.json"
if [ -f "$state" ]; then
  jq -s '.[0] + .[1]' "$DOTFILES/herdr-sidebar/state.json" "$state" > "$state.tmp" && mv "$state.tmp" "$state"
else
  cp "$DOTFILES/herdr-sidebar/state.json" "$state"
fi
printf '%s {file}' "$BIN/fresh-open" > "$state_dir/editor-command.txt"
info "herdr-sidebar: click opens in Fresh ($(cat "$state_dir/editor-command.txt"))"

if [ "$with_claude" = 1 ] && [ -d "$HOME/.claude" ]; then
  herdr integration install claude >/dev/null && info "Claude Code hook installed (herdr integration status)"
fi

# herdr gets an agent's state from these hooks. pi has "full lifecycle hook
# authority", so herdr skips its screen heuristics for it: without the
# integration the spaces rail shows pi idle forever. Rerun after a pi upgrade.
if [ -d "$HOME/.pi" ]; then
  herdr integration install pi >/dev/null && info "pi agent-state hook installed (herdr integration status)"
fi

herdr server reload-config >/dev/null 2>&1 && info "reloaded the running herdr server" || true

# ── 5. Language servers (default; --no-lsp skips) ─────────────────────
if [ "$with_lsp" = 1 ]; then
  step "language servers"
  lsp_args=()
  [ "$with_packages" = 1 ] || lsp_args+=(--no-apt)
  "$DOTFILES/install-lsp.sh" ${lsp_args[@]+"${lsp_args[@]}"} ||
    info "some language servers failed (see the summary above; rerun dotfiles/install-lsp.sh)"
fi

# ── 6. Login shell ─────────────────────────────────────────────────────
if [ "$(getent passwd "$USER" | cut -d: -f7)" != "$(command -v zsh)" ]; then
  step "login shell"
  chsh -s "$(command -v zsh)"
fi

step "done"
info "open a new shell (or: exec zsh), then run: herdr"
info "in a project pane: ctrl+b i -> IDE layout, ctrl+b ? -> all keys"
[ -d "$BACKUP" ] && info "replaced files are in $BACKUP"
exit 0
