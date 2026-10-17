#!/usr/bin/env bash
# install-herdr-ide.sh — one-shot install of the herdr + Fresh IDE on WSL Ubuntu.
#
#   docs/install-herdr-ide.sh                 full install (sudo for apt when needed)
#   docs/install-herdr-ide.sh --lsp           also language servers (passed to dotfiles)
#   docs/install-herdr-ide.sh --no-packages   skip apt (passed to dotfiles)
#   docs/install-herdr-ide.sh --no-claude     skip the Claude Code hook (passed to dotfiles)
#   docs/install-herdr-ide.sh --no-oco        skip OpenCommit (oco) for the AI commit key
#   docs/install-herdr-ide.sh -h|--help       this text
#
# Idempotent: rerun after pulling. Replaced configs go to ~/.dotfiles-backup/<ts>/.
set -euo pipefail

BIN="$HOME/.local/bin"
export PATH="$BIN:$PATH"

with_oco=1
passthrough=()
while [ $# -gt 0 ]; do
  case $1 in
    --lsp|--no-packages|--no-claude) passthrough+=("$1") ;;
    --no-oco) with_oco=0 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

step() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '\033[1;33m    %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
ok()   { printf '    \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '    \033[31m✗\033[0m %s\n' "$*"; }

# ── 0. Preflight ───────────────────────────────────────────────────────
step "preflight"
[ "$(id -u)" -ne 0 ] || die "run as your user, not root"
grep -qi microsoft /proc/version 2>/dev/null || warn "note: not WSL; Windows bridging (wsl-open) will be inert"

REPO=$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)
PLUGIN=$REPO/plugins/herdr-sidebar
[ -f "$PLUGIN/herdr-plugin.toml" ] || die "herdr-sidebar plugin not found at $PLUGIN"
DOTFILES=$REPO/dotfiles
[ -f "$DOTFILES/install.sh" ] || die "dotfiles/ missing in $REPO"
info "sidebar source: $REPO"
info "dotfiles:       $DOTFILES"

# ── 1. Config drift check ──────────────────────────────────────────────
# Fresh's settings UI and herdr can replace the dotfiles symlink with a copy.
# The dotfiles installer would back that copy up and re-link, silently
# dropping any setting made only through the UI.
drift_check() {
  local src=$1 dest=$2
  if [ -L "$dest" ] && [ "$(readlink "$dest")" = "$src" ]; then return 0; fi
  [ -e "$dest" ] || [ -L "$dest" ] || return 0
  step "config drift: $dest"
  warn "not a symlink to $src (a settings UI probably replaced it with a copy)"
  diff -u "$src" "$dest" || true
  warn "the dotfiles installer will back it up and re-link, so any setting shown"
  warn "in the diff must be merged into dotfiles by hand first."
}
step "config drift check"
drift_check "$DOTFILES/config/fresh/config.json" "$HOME/.config/fresh/config.json"
drift_check "$DOTFILES/config/herdr/config.toml" "$HOME/.config/herdr/config.toml"

# ── 2. Dotfiles installer (packages, herdr, Fresh, configs, plugins) ───
step "dotfiles installer"
"$DOTFILES/install.sh" ${passthrough[@]+"${passthrough[@]}"}

# ── 3. OpenCommit (oco) for the Ctrl+Shift+A AI commit ─────────────────
if [ "$with_oco" = 1 ]; then
  step "OpenCommit (oco)"
  if command -v oco >/dev/null; then
    info "oco present"
  else
    if ! command -v npm >/dev/null; then
      sudo apt-get install -y -qq nodejs npm
    fi
    # The global npm prefix is root-owned, hence the user prefix.
    npm install -g --prefix "$HOME/.local" opencommit
  fi
  info "oco version: $(oco --version 2>/dev/null || echo unknown)"
else
  step "OpenCommit (oco)"
  info "skipped (--no-oco)"
fi

# ── 4. Verify (report only; never aborts) ──────────────────────────────
step "verify"
actions=$(herdr plugin action list 2>/dev/null || true)
for a in git-commit-ai git-pull git-push file-search search-popup replace-popup git-switch-branch restart-fresh; do
  if grep -qE "\"action_id\": *\"$a\"" <<<"$actions"; then
    ok "herdr action $a"
  else
    bad "herdr action $a missing"
  fi
done

for p in selection_share.ts git_line_history.ts; do
  if [ -L "$HOME/.config/fresh/plugins/$p" ] || [ -e "$HOME/.config/fresh/plugins/$p" ]; then
    ok "fresh plugin $p"
  else
    bad "fresh plugin $p missing (~/.config/fresh/plugins/$p)"
  fi
done

fresh_cfg=$(fresh --cmd config show 2>/dev/null || true)
for k in selection_send_to_agent git_line_history recent_tab conflict_accept_current; do
  if grep -q "$k" <<<"$fresh_cfg"; then
    ok "fresh config mentions $k"
  else
    bad "fresh config does not mention $k"
  fi
done

fresh_link=$HOME/.config/fresh/config.json
if [ -L "$fresh_link" ] && [ "$(readlink "$fresh_link")" = "$DOTFILES/config/fresh/config.json" ]; then
  ok "fresh config.json is the dotfiles symlink"
else
  bad "fresh config.json is not a symlink to $DOTFILES/config/fresh/config.json (Fresh's settings screen replaced it; merge and rerun)"
fi

if command -v lazygit >/dev/null 2>&1; then
  ok "lazygit present (ctrl+b alt+g)"
else
  bad "lazygit missing (sudo apt install lazygit)"
fi

if [ "$with_oco" = 1 ]; then
  if command -v node >/dev/null 2>&1; then
    ok "node present (oco's hook mode needs it)"
  else
    bad "node missing (oco's hook mode needs it; sudo apt install nodejs)"
  fi
  # Read only the provider line; the API key is never printed.
  provider=$(sed -n 's/^OCO_AI_PROVIDER=//p' "$HOME/.opencommit" 2>/dev/null | tail -n1 || true)
  if [ -n "$provider" ] && [ "$provider" != undefined ]; then
    ok "oco has an AI provider configured"
  else
    bad "oco has no AI provider (run oco setup)"
  fi
fi

plugin_root() {
  herdr plugin list --json 2>/dev/null |
    jq -r --arg id "$1" '.result.plugins[]? | select(.plugin_id==$id) | .plugin_root' 2>/dev/null || true
}
for entry in "herdr-sidebar:$PLUGIN" "dotfiles-ide:$DOTFILES/herdr-plugins/ide"; do
  id=${entry%%:*} want=${entry#*:}
  got=$(plugin_root "$id")
  if [ "$got" = "$want" ]; then
    ok "$id runs $want"
  else
    bad "$id runs ${got:-nothing} (expected $want)"
  fi
done

if herdr server reload-config >/dev/null 2>&1; then
  ok "reloaded the running herdr server"
else
  info "no running herdr server to reload (fine)"
fi

# ── 5. Next steps ──────────────────────────────────────────────────────
step "next steps"
[ "$with_oco" = 1 ] && info "1. oco setup   (once: choose the AI provider and key for the AI commit)"
info "2. Restart Fresh so plugins and keys load (graceful: unsaved buffers come back):"
info "     $DOTFILES/herdr-plugins/ide/bin/fresh-restart --all"
info "3. Windows Terminal binds Ctrl+Shift+A to 'Select all' and Ctrl+Tab to 'next tab' by"
info "   default (Pebrel too): remove both, or the AI-commit and recent-tab keys never reach"
info "   herdr and Fresh."
info "4. Start: herdr, then in a project pane press ctrl+b i for the IDE layout."
info "   Full docs: $REPO/docs/herdr-ide.md"
if [ -d "$HOME/code/dotfiles" ] && [ "$(cd "$HOME/code/dotfiles" && pwd)" != "$DOTFILES" ]; then
  info "The old standalone $HOME/code/dotfiles checkout is no longer used by anything"
  info "once this has run; archive or delete it."
fi
[ -d "$HOME/.dotfiles-backup" ] && info "5. Replaced files were backed up under ~/.dotfiles-backup/"
exit 0
