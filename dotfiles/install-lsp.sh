#!/usr/bin/env bash
# shellcheck disable=SC2329  # installers are called by name from the servers table
# install-lsp.sh — language servers for the Fresh editor (herdr + Fresh IDE in WSL).
#
# Installs marksman (Markdown), gopls (Go), rust-analyzer (Rust), the Dart SDK's
# language server, phpactor (PHP), the npm servers (TypeScript/JavaScript, Bash,
# YAML, JSON) and python-lsp-server (Python). Normally run by dotfiles/install.sh.
#
#   dotfiles/install-lsp.sh                install what is missing
#   dotfiles/install-lsp.sh --update       reinstall/upgrade everything it manages
#   dotfiles/install-lsp.sh --only LIST    only these languages: markdown,go,rust,dart,php,js,python
#   dotfiles/install-lsp.sh --no-apt       never call apt (servers that need apt are reported failed)
#   dotfiles/install-lsp.sh -h|--help      this text
#
# Idempotent: a server whose command already works is skipped unless --update.
# Binaries go to ~/.local/bin, the Dart SDK to ~/.local/share/dart-sdk, npm
# packages to the user prefix ~/.local (the global npm prefix is root-owned).
# A failing server is reported and the rest still install. Exit 1 only when
# every wanted server failed.
set -euo pipefail

BIN="$HOME/.local/bin"
export PATH="$BIN:$HOME/.cargo/bin:$PATH"
mkdir -p "$BIN"

update=0 no_apt=0 only=""
while [ $# -gt 0 ]; do
  case $1 in
    --update) update=1 ;;
    --no-apt) no_apt=1 ;;
    --only) shift; only=${1:-}; [ -n "$only" ] || { echo "--only needs a list" >&2; exit 2; } ;;
    --only=*) only=${1#--only=} ;;
    -h|--help) sed -n '3,19p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

valid="markdown go rust dart php js python"
for l in ${only//,/ }; do
  case " $valid " in *" $l "*) ;; *) echo "unknown language in --only: $l (valid: ${valid// /,})" >&2; exit 2 ;; esac
done

step() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '\033[1;33m    %s\033[0m\n' "$*"; }

case $(uname -m) in
  x86_64)        ARCH=x64;   RUST_ARCH=x86_64 ;;
  aarch64|arm64) ARCH=arm64; RUST_ARCH=aarch64 ;;
  *)             ARCH=;      RUST_ARCH= ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }

# wanted LANG — true when LANG is selected by --only (or no --only).
wanted() { [ -z "$only" ] || [[ " ${only//,/ } " == *" $1 "* ]]; }

# need CMD — true when CMD should be installed or upgraded now.
need() { [ "$update" = 1 ] || ! command -v "$1" >/dev/null 2>&1; }

# ── 1. Decide the apt packages ─────────────────────────────────────────
apt_pkgs=()
if wanted go && need go; then apt_pkgs+=(golang-go); fi
if wanted rust && ! command -v rustup >/dev/null 2>&1; then
  # rust-src lets rust-analyzer complete std with a distro toolchain.
  # No grep -q: it exits early, apt-cache gets SIGPIPE and pipefail fails the test.
  if apt-cache policy rust-src 2>/dev/null | grep 'Candidate: [^(]' >/dev/null; then
    if [ "$update" = 1 ] || ! dpkg -s rust-src >/dev/null 2>&1; then apt_pkgs+=(rust-src); fi
  fi
fi
if wanted dart && ! command -v unzip >/dev/null 2>&1; then apt_pkgs+=(unzip); fi
if wanted php && need php; then apt_pkgs+=(php-cli php-xml php-mbstring); fi
if wanted js && need npm; then apt_pkgs+=(npm); fi
if wanted python && need pylsp; then apt_pkgs+=(python3-pylsp); fi

if [ "${#apt_pkgs[@]}" -gt 0 ]; then
  mapfile -t apt_pkgs < <(printf '%s\n' "${apt_pkgs[@]}" | sort -u)
  step "apt packages: ${apt_pkgs[*]}"
  if [ "$no_apt" = 1 ]; then
    warn "skipped (--no-apt)"
  else
    sudo apt-get update -qq || warn "apt-get update failed; trying the install anyway"
    sudo apt-get install -y -qq "${apt_pkgs[@]}" || warn "apt install failed; servers that need it will fail"
  fi
fi

# ── 2. Installers (each returns non-zero on failure, never exits) ──────
# Every step checks its own status: set -e is off inside a condition context.

install_marksman() {
  local d=$tmp/marksman f
  mkdir -p "$d"
  case $ARCH in
    x64)   f=marksman-linux-x64 ;;
    arm64) f=marksman-linux-arm64 ;;
    *) warn "no marksman build for $(uname -m)"; return 1 ;;
  esac
  fetch "https://github.com/artempyanykh/marksman/releases/latest/download/$f" "$d/marksman" || return 1
  chmod +x "$d/marksman" && mv -f "$d/marksman" "$BIN/marksman"
}

install_gopls() {
  command -v go >/dev/null 2>&1 || { warn "go missing (apt golang-go)"; return 1; }
  GOBIN="$BIN" go install golang.org/x/tools/gopls@latest
}

install_rust_analyzer() {
  if command -v rustup >/dev/null 2>&1; then
    rustup component add rust-analyzer rust-src
    return
  fi
  [ -n "$RUST_ARCH" ] || { warn "no rust-analyzer build for $(uname -m)"; return 1; }
  local d=$tmp/rust-analyzer
  mkdir -p "$d"
  # rust-analyzer releases since 2026-07-27 refuse toolchains older than 1.94
  # (MINIMUM_SUPPORTED_TOOLCHAIN_VERSION). A distro rustc lags behind, so pin
  # the last release that still accepts it (it supports 1.78 and later).
  local minor release=latest/download
  minor=$(rustc --version 2>/dev/null | sed -n 's/^rustc 1\.\([0-9]*\).*/\1/p')
  if [ -n "$minor" ] && [ "$minor" -lt 94 ]; then release=download/2026-07-20; fi
  fetch "https://github.com/rust-lang/rust-analyzer/releases/$release/rust-analyzer-$RUST_ARCH-unknown-linux-gnu.gz" "$d/ra.gz" || return 1
  gunzip -c "$d/ra.gz" > "$d/rust-analyzer" || return 1
  chmod +x "$d/rust-analyzer" && mv -f "$d/rust-analyzer" "$BIN/rust-analyzer"
}

# Returns 3 when another Dart SDK (e.g. Flutter's) is first on PATH: left alone.
install_dart() {
  local ours=$HOME/.local/share/dart-sdk dart_path real_ours real_path
  dart_path=$(command -v dart 2>/dev/null || true)
  if [ -n "$dart_path" ]; then
    real_path=$(readlink -f "$dart_path")
    real_ours=$(readlink -f "$ours/bin/dart" 2>/dev/null || echo "$ours/bin/dart")
    if [ "$real_path" != "$real_ours" ]; then return 3; fi
  fi
  [ -n "$ARCH" ] || { warn "no Dart SDK build for $(uname -m)"; return 1; }
  local base=https://storage.googleapis.com/dart-archive/channels/stable/release/latest/sdk
  local zip=dartsdk-linux-$ARCH-release.zip d=$tmp/dart expected actual stage
  mkdir -p "$d/unpack"
  fetch "$base/$zip" "$d/$zip" || return 1
  fetch "$base/$zip.sha256sum" "$d/$zip.sha256sum" || return 1
  # Format: "<hash> *<filename>"
  expected=$(awk 'NR==1{print $1}' "$d/$zip.sha256sum")
  actual=$(sha256sum "$d/$zip" | awk '{print $1}')
  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    warn "Dart SDK checksum mismatch; not installed"
    return 1
  fi
  unzip -q "$d/$zip" -d "$d/unpack" || return 1
  [ -d "$d/unpack/dart-sdk" ] || { warn "Dart SDK zip has no dart-sdk directory"; return 1; }
  chmod +x "$d/unpack/dart-sdk/bin/"* || return 1
  # Swap in the new SDK with two renames so a failure leaves the old one in place.
  mkdir -p "$HOME/.local/share"
  stage=$HOME/.local/share/dart-sdk.new
  rm -rf "$stage"
  mv "$d/unpack/dart-sdk" "$stage" || return 1
  rm -rf "$HOME/.local/share/dart-sdk.old"
  if [ -e "$ours" ]; then mv "$ours" "$HOME/.local/share/dart-sdk.old" || return 1; fi
  mv "$stage" "$ours" || return 1
  rm -rf "$HOME/.local/share/dart-sdk.old"
  ln -sfn "$ours/bin/dart" "$BIN/dart"
}

install_phpactor() {
  command -v php >/dev/null 2>&1 || { warn "php missing (apt php-cli php-xml php-mbstring)"; return 1; }
  local d=$tmp/phpactor
  mkdir -p "$d"
  fetch "https://github.com/phpactor/phpactor/releases/latest/download/phpactor.phar" "$d/phpactor" || return 1
  chmod +x "$d/phpactor" && mv -f "$d/phpactor" "$BIN/phpactor"
}

# The four npm servers share ONE npm call per run.
npm_rc=""
install_npm_servers() {
  if [ -z "$npm_rc" ]; then
    npm_rc=1
    if command -v npm >/dev/null 2>&1; then
      npm install -g --prefix "$HOME/.local" --silent \
        typescript typescript-language-server bash-language-server \
        vscode-langservers-extracted yaml-language-server && npm_rc=0
    else
      warn "npm missing (apt npm)"
    fi
  fi
  return "$npm_rc"
}

# Python's server comes from apt in the step above; nothing more to do here.
install_apt_only() { return 0; }

# ── 3. Per-server driver and summary ───────────────────────────────────
# Columns: server, command, language, installer
servers=(
  "marksman|marksman|markdown|install_marksman"
  "gopls|gopls|go|install_gopls"
  "rust-analyzer|rust-analyzer|rust|install_rust_analyzer"
  "dart language-server|dart|dart|install_dart"
  "phpactor|phpactor|php|install_phpactor"
  "typescript-language-server|typescript-language-server|js|install_npm_servers"
  "bash-language-server|bash-language-server|js|install_npm_servers"
  "yaml-language-server|yaml-language-server|js|install_npm_servers"
  "vscode-json-language-server|vscode-json-language-server|js|install_npm_servers"
  "pylsp|pylsp|python|install_apt_only"
)

rows=()
record() { rows+=("$(printf '%-28s %-28s %s' "$1" "$2" "$3")"); }

for entry in "${servers[@]}"; do
  IFS='|' read -r name cmd lang fn <<<"$entry"
  wanted "$lang" || continue
  if [ "$update" = 0 ] && path=$(command -v "$cmd" 2>/dev/null); then
    if [ "$cmd" = dart ] && [ "$(readlink -f "$path")" != "$(readlink -f "$HOME/.local/share/dart-sdk/bin/dart" 2>/dev/null || true)" ]; then
      record "$name" "$cmd" "present (other SDK: $path)"
    else
      record "$name" "$cmd" "present"
    fi
    continue
  fi
  step "$name ($cmd)"
  rc=0
  "$fn" || rc=$?
  if [ "$rc" = 3 ]; then
    record "$name" "$cmd" "present (other SDK: $(command -v dart))"
  elif [ "$rc" = 0 ] && command -v "$cmd" >/dev/null 2>&1; then
    record "$name" "$cmd" "installed"
  else
    warn "failed: $name"
    record "$name" "$cmd" "failed"
  fi
done

if [ "${#rows[@]}" -eq 0 ]; then
  info "nothing selected"
  exit 0
fi

step "summary"
printf '    %-28s %-28s %s\n' "server" "command" "status"
for r in "${rows[@]}"; do printf '    %s\n' "$r"; done

# Exit 1 only when every wanted server failed.
for r in "${rows[@]}"; do
  [[ $r == *' failed' ]] || exit 0
done
exit 1
