#!/usr/bin/env bash
# herdr-sidebar/build.sh — build and link THIS checkout's herdr-sidebar plugin.
#
# The fork, the plugin and these dotfiles live in one repo, so there are no
# upstream patches. Update = `git pull`, then rerun (install or this script).
# Needs cargo and rustc (Ubuntu: apt install rustc cargo) and jq.
set -euo pipefail

HERE=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
PLUGIN=$REPO/plugins/herdr-sidebar
export PATH="$HOME/.local/bin:$PATH"

command -v cargo >/dev/null || { echo "cargo missing: sudo apt install rustc cargo" >&2; exit 1; }

(cd "$PLUGIN" && cargo build --release --quiet)

# Point herdr's registration at this checkout (re-link if it was another one).
plugins=$(herdr plugin list --json 2>/dev/null || echo '{}')
registered=$(jq -r '.result.plugins[]? | select(.plugin_id=="herdr-sidebar") | .plugin_root' <<<"$plugins" 2>/dev/null || true)
if [ -n "$registered" ] && [ "$registered" != "$PLUGIN" ]; then
  kind=$(jq -r '.result.plugins[]? | select(.plugin_id=="herdr-sidebar") | .source.kind' <<<"$plugins" 2>/dev/null || true)
  if [ "$kind" = local ]; then
    herdr plugin unlink herdr-sidebar >/dev/null
  else
    herdr plugin uninstall herdr-sidebar >/dev/null
  fi
  herdr plugin link "$PLUGIN" >/dev/null
elif [ -z "$registered" ]; then
  herdr plugin link "$PLUGIN" >/dev/null
fi

# Running sidebars keep the old binary until they respawn.
herdr plugin action invoke herdr-sidebar.redeploy >/dev/null 2>&1 || true
echo "herdr-sidebar built from $REPO ($(git -C "$REPO" describe --tags --always 2>/dev/null || echo unknown))"
