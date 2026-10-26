#!/usr/bin/env bash
# verify.sh — is THIS machine still in sync with the repo?
#
#   ./verify.sh            full report; exit 1 if the workbench is broken
#   ./verify.sh --quiet    print only problems
#   ./verify.sh --plan     print the checklist source (install.sh --dry-run) and exit
#
# Read-only: it never installs, links, reloads, or writes anything. The
# checklist is the PLAN output of `./install.sh --dry-run`, so a new link, tool
# or plugin is verified the moment it is added to install.sh — there is
# deliberately no second manifest that could drift out of sync with the
# installer. Everything here answers one question: "after `git pull`, is this
# machine actually running what the repo describes?"
set -uo pipefail

DOTFILES=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
quiet=0 plan_only=0
for arg in "$@"; do
  case $arg in
    --quiet|-q) quiet=1 ;;
    --plan) plan_only=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

ok=0 warn=0 fail=0
pass() { ok=$((ok + 1)); [ "$quiet" = 1 ] || printf '  \033[32m✓\033[0m %s\n' "$*"; }
note() { warn=$((warn + 1)); printf '  \033[33m!\033[0m %s\n' "$*"; }
bad() { fail=$((fail + 1)); printf '  \033[31m✗\033[0m %s\n' "$*"; }
group() { [ "$quiet" = 1 ] || printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

# A tool the workbench cannot run without is a failure; the rest is a warning,
# so a missing `btop` never makes the report useless.
essential() {
  case $1 in
    herdr | fresh | zsh | git | jq) return 0 ;;
    *) return 1 ;;
  esac
}

group "checklist"
# --no-packages/--no-lsp keep it fast; neither affects the PLAN lines (tool and
# link entries are emitted unconditionally), and verify never installs anything.
plan=$("$DOTFILES/install.sh" --dry-run --no-packages --no-lsp 2>&1)
if [ $? -ne 0 ]; then
  bad "install.sh --dry-run failed, so nothing below could be checked"
  printf '%s\n' "$plan" | sed 's/^/      /'
  exit 1
fi
if [ "$plan_only" = 1 ]; then
  printf '%s\n' "$plan" | grep '^PLAN' || true
  exit 0
fi
n_plan=$(printf '%s\n' "$plan" | grep -c '^PLAN' || true)
pass "$n_plan managed artifacts listed by install.sh"

herdr_plugins() { herdr plugin list --json 2>/dev/null || echo '{}'; }

group "links (one repo file per machine path)"
while IFS=$'\t' read -r _ kind dest src; do
  case $kind in
    LINK)
      if [ ! -e "$src" ]; then
        bad "$dest -> $src, which does not exist in the repo"
      elif [ ! -L "$dest" ]; then
        if [ -e "$dest" ]; then
          bad "$dest is a copy, not a symlink — a 'git pull' will not reach it (rerun install.sh to back it up and link it)"
        else
          bad "$dest is missing (rerun install.sh)"
        fi
      else
        got=$(readlink "$dest")
        if [ "$got" = "$src" ]; then
          pass "$dest"
        else
          bad "$dest points at $got (expected $src)"
        fi
      fi
      ;;
    OVERLAY)
      # ~/.zshrc.local is the one machine-local seam the repo does not manage:
      # the managed rc must keep sourcing it, or those files go dead silently.
      if [ ! -e "$dest" ]; then
        pass "no ~/.zshrc.local (fully managed shell)"
      elif grep -qF '.zshrc.local' "$DOTFILES/zsh/zshrc"; then
        pass "$dest is sourced by the managed rc"
      else
        bad "$dest exists but dotfiles/zsh/zshrc never sources it"
      fi
      ;;
    UVTOOL)
      # dest = tool name, src = the version pinned in dotfiles/packages.lock.
      have=$(command -v uv >/dev/null 2>&1 && uv tool list 2>/dev/null |
        awk -v n="$dest" '$1 == n { sub(/^v/, "", $2); print $2; exit }')
      if [ "$have" = "$src" ]; then
        pass "uv tool $dest $src"
      elif [ -z "$have" ]; then
        bad "uv tool $dest is not installed (packages.lock pins $src; run: uv tool install '$dest==$src')"
      else
        bad "uv tool $dest is $have but packages.lock pins $src (rerun install.sh)"
      fi
      ;;
    UVWITH)
      # dest = tool, src = a `--with` requirement pinned in packages.lock.
      line=$(command -v uv >/dev/null 2>&1 && uv tool list --show-with 2>/dev/null |
        awk -v n="$dest" 'index($0, n" v") == 1 { print; exit }')
      if [ -n "$line" ] && printf '%s\n' "$line" | grep -qF "$src"; then
        pass "uv tool $dest carries $src"
      else
        bad "uv tool $dest is missing $src (rerun install.sh)"
      fi
      ;;
    GENERATED)
      if [ -s "$dest" ]; then pass "$dest"; else bad "$dest missing or empty (rerun install.sh)"; fi
      ;;
    STATE)
      if [ ! -s "$dest" ]; then
        bad "$dest missing (rerun install.sh)"
      elif jq -e -s '.[0] as $live | .[1] as $seed | ($seed | keys) - ($live | keys) | length == 0' \
        "$dest" "$DOTFILES/herdr-sidebar/state.json" >/dev/null 2>&1; then
        pass "$dest (every seeded key present)"
      else
        missing=$(jq -r -s '.[0] as $live | .[1] as $seed | ($seed | keys) - ($live | keys) | join(", ")' \
          "$dest" "$DOTFILES/herdr-sidebar/state.json" 2>/dev/null || true)
        bad "$dest is missing seeded settings: ${missing:-?} (rerun install.sh)"
      fi
      ;;
    EDITOR-COMMAND)
      cmd_file="$HOME/.local/state/herdr/plugins/herdr-sidebar/editor-command.txt"
      if [ ! -s "$cmd_file" ]; then
        bad "$cmd_file missing (rerun install.sh)"
      elif [ "$(cat "$cmd_file")" = "$dest" ]; then
        pass "editor command: $dest"
      else
        bad "editor command is '$(cat "$cmd_file")' (expected '$dest')"
      fi
      ;;
  esac
done < <(printf '%s\n' "$plan" | grep '^PLAN')

group "tools on PATH"
while IFS=$'\t' read -r _ kind cmd; do
  [ "$kind" = TOOL ] || continue
  if command -v "$cmd" >/dev/null 2>&1; then
    pass "$cmd"
  elif essential "$cmd"; then
    bad "$cmd missing (the workbench cannot run without it)"
  else
    note "$cmd missing (optional; rerun install.sh)"
  fi
done < <(printf '%s\n' "$plan" | grep '^PLAN')

group "herdr plugins"
while IFS=$'\t' read -r _ kind id want; do
  [ "$kind" = PLUGIN ] || continue
  got=$(herdr_plugins | jq -r --arg id "$id" '.result.plugins[]? | select(.plugin_id==$id) | .plugin_root' 2>/dev/null | head -n1)
  if [ "$got" = "$want" ]; then
    pass "$id -> $want"
  elif [ -z "$got" ]; then
    bad "$id is not registered (run install.sh, or: herdr plugin link $want)"
  else
    bad "$id is registered from $got (expected $want)"
  fi
done < <(printf '%s\n' "$plan" | grep '^PLAN')

group "herdr integrations (agent state)"
while IFS=$'\t' read -r _ kind agent; do
  [ "$kind" = INTEGRATION ] || continue
  line=$(herdr integration status 2>/dev/null | grep -E "^$agent:" || true)
  case $line in
    *current*) pass "$agent: ${line#*: }" ;;
    *outdated*) note "$agent hook is outdated: ${line#*: } (rerun install.sh)" ;;
    *) bad "$agent hook not installed: ${line:-herdr did not report it}" ;;
  esac
done < <(printf '%s\n' "$plan" | grep '^PLAN')

group "git"
while IFS=$'\t' read -r _ kind path; do
  [ "$kind" = GIT-INCLUDE ] || continue
  if git config --global --get-all include.path 2>/dev/null | grep -qxF "$path"; then
    pass "global gitconfig includes $path"
  else
    bad "global gitconfig does not include $path (rerun install.sh)"
  fi
done < <(printf '%s\n' "$plan" | grep '^PLAN')

group "keymap (managed here: config.toml + scripts/)"
cfg="$DOTFILES/config/herdr/config.toml"
if [ ! -f "$cfg" ]; then
  bad "$cfg missing from the repo"
else
  bindings=$(grep -c '^\[\[keys\.command\]\]' "$cfg" || true)
  refs=$(grep -oE 'scripts/[A-Za-z0-9._-]+\.sh' "$cfg" | sort -u || true)
  missing=0
  while read -r ref; do
    [ -n "$ref" ] || continue
    [ -f "$DOTFILES/config/herdr/$ref" ] || {
      bad "keymap runs $ref, which is not in dotfiles/config/herdr/"
      missing=1
    }
  done <<<"$refs"
  n_refs=$(printf '%s\n' "$refs" | grep -c . || true)
  [ "$missing" = 0 ] && pass "$bindings bindings, all $n_refs script references resolve"
  if herdr config check >/dev/null 2>&1; then
    pass "herdr config check: ok"
  else
    bad "herdr config check failed (herdr config check)"
  fi
fi

# CodeBuddy's agent state is our own hook (herdr has no integration for it), so
# it is verified only where CodeBuddy exists.
if command -v codebuddy >/dev/null 2>&1 || [ -d "$HOME/.codebuddy" ]; then
  cb_hook=$HOME/.codebuddy/hooks/herdr-agent-state.sh
  if [ -x "$cb_hook" ]; then
    pass "codebuddy hook script"
  else
    bad "codebuddy hook script missing or not executable ($cb_hook)"
  fi
  if jq -e --arg h "$cb_hook" \
    '[.hooks[]?[]? | .hooks[]? | select(.command == $h)] | length >= 5' \
    "$HOME/.codebuddy/settings.json" >/dev/null 2>&1; then
    pass "codebuddy settings run the agent-state hook"
  else
    bad "codebuddy settings do not run the agent-state hook (rerun install.sh)"
  fi
fi

group "result"
printf '  %d ok, %d warning(s), %d failure(s)\n' "$ok" "$warn" "$fail"
if [ "$fail" -ne 0 ]; then
  printf '  this machine is out of sync: run dotfiles/install.sh (or docs/install-herdr-ide.sh)\n'
  exit 1
fi
printf '  in sync with %s\n' "$DOTFILES"
