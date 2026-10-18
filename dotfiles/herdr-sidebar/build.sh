#!/usr/bin/env bash
# herdr-sidebar/build.sh — build and link THIS checkout's herdr-sidebar plugin.
#
# The fork, the plugin and these dotfiles live in one repo, so there are no
# upstream patches. Update = `git pull`, then rerun (install or this script).
# Needs cargo and rustc at least as new as the crate's rust-version (Ubuntu 24.04's
# apt Rust is too old, so a rustup toolchain is used). Needs jq.
set -euo pipefail

HERE=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
PLUGIN=$REPO/plugins/herdr-sidebar
export PATH="$HOME/.local/bin:$PATH"

# The toolchain must be at least the crate's rust-version (currently 1.89).
need=$(sed -n 's/^rust-version *= *"\([0-9.]*\)".*/\1/p' "$PLUGIN/Cargo.toml" | head -n1)
[ -n "$need" ] || { echo "no rust-version in $PLUGIN/Cargo.toml" >&2; exit 1; }

# rust_ok — cargo and a rustc on PATH that are >= $need (sort -V picks the older one).
rust_ok() {
  local have
  command -v cargo >/dev/null 2>&1 || return 1
  command -v rustc >/dev/null 2>&1 || return 1
  have=$(rustc --version 2>/dev/null | awk '{print $2}') || true
  have=${have%%-*}
  [ -n "$have" ] && [ "$(printf '%s\n%s\n' "$need" "$have" | sort -V | head -n1)" = "$need" ]
}

# 1. Whatever is on PATH; 2. an existing rustup install; 3. install rustup into $HOME.
if rust_ok; then
  how="system rust"
elif [ -x "$HOME/.cargo/bin/rustc" ]; then
  export PATH="$HOME/.cargo/bin:$PATH"
  if ! rust_ok && command -v rustup >/dev/null 2>&1; then
    rustup update stable >/dev/null
  fi
  how="rustup in ~/.cargo/bin"
else
  echo "Rust >= $need needed: installing rustup into ~/.cargo (non-interactive)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
  export PATH="$HOME/.cargo/bin:$PATH"
  how="rustup installed into ~/.cargo"
fi
rust_ok || { echo "no cargo/rustc >= $need found (rustc: $(rustc --version 2>/dev/null || echo none))" >&2; exit 1; }
echo "using $how: $(rustc --version)"

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
