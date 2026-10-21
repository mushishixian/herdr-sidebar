# Shared helpers for the dotfiles-ide herdr plugin. Sourced, not executed.
#
# The IDE layout is identified by pane labels, not ids, so it survives
# herdr restarts:
#   "Fresh"    – the editor pane (also carries a fresh_daemon metadata token)
#   "Terminal" – the shell pane under the editor
#   "Agent"    – the coding agent column on the right
#   "Sidebar"  – owned by the herdr-sidebar plugin; never touched here

HERDR=${HERDR_BIN_PATH:-herdr}
FRESH_LABEL=Fresh
TERM_LABEL=Terminal
AGENT_LABEL=Agent
TOKEN_SOURCE=dotfiles-ide

# Ids of the caller. Plugin actions get HERDR_*_ID, custom keybindings get
# HERDR_ACTIVE_*_ID, a shell inside a pane gets HERDR_*_ID.
cur_ws=${HERDR_WORKSPACE_ID:-${HERDR_ACTIVE_WORKSPACE_ID:-}}
cur_tab=${HERDR_TAB_ID:-${HERDR_ACTIVE_TAB_ID:-}}
cur_pane=${HERDR_PANE_ID:-${HERDR_ACTIVE_PANE_ID:-}}

warn() {
  printf 'dotfiles-ide: %s\n' "$*" >&2
  "$HERDR" notification show "IDE" --body "$*" >/dev/null 2>&1 || true
}

die() { warn "$@"; exit 1; }

in_herdr() { [ -n "${HERDR_SOCKET_PATH:-}${HERDR_ENV:-}" ] && "$HERDR" status server >/dev/null 2>&1; }

# Git toplevel of $1, or $1 itself outside a repository.
project_root() {
  git -C "$1" rev-parse --show-toplevel 2>/dev/null || (cd "$1" 2>/dev/null && pwd -P) || printf '%s\n' "$1"
}

# Fresh keys its socket on the daemon name. A per-directory daemon embeds the
# whole path, which overflows sun_path (108 bytes) for deep checkouts, so
# every project gets a short, stable name instead.
daemon_name() {
  printf 'ide-%s\n' "$(printf '%s' "$1" | sha1sum | cut -c1-10)"
}

daemon_alive() {
  fresh --cmd daemon list 2>/dev/null | sed -n 's/^  //p' | grep -qx -- "$1"
}

# True when pid $1's argv is exactly `fresh ARG...` (argv[0] may be a full path
# to the binary). Compares whole arguments, so `ide-abc` never matches
# `ide-abc-s2`. pgrep only pre-filters; this is the real test.
fresh_argv_is() { # pid arg...
  local pid=$1 i
  shift
  local -a want=("$@") argv=()
  mapfile -d '' -t argv < <(cat "/proc/$pid/cmdline" 2>/dev/null)
  [ "${#argv[@]}" -eq $((${#want[@]} + 1)) ] && [ "${argv[0]##*/}" = fresh ] || return 1
  for i in "${!want[@]}"; do
    [ "${argv[i + 1]}" = "${want[i]}" ] || return 1
  done
}

# One line per Fresh SERVER process: "NAME PID" (the server is
# `fresh --server --session-name NAME`, a process group leader whose language
# servers share its group). Empty when none runs.
fresh_server_list() {
  local pid
  local -a argv
  for pid in $(pgrep -f -- '--server --session-name' 2>/dev/null); do
    mapfile -d '' -t argv < <(cat "/proc/$pid/cmdline" 2>/dev/null)
    # Exactly four arguments: fresh --server --session-name NAME.
    [ "${#argv[@]}" -eq 4 ] && fresh_argv_is "$pid" --server --session-name "${argv[3]}" \
      && printf '%s %s\n' "${argv[3]}" "$pid"
  done
  return 0
}

# PID of the Fresh server for daemon $1, or nothing.
fresh_server_pid() {
  fresh_server_list | awk -v n="$1" '$1 == n { print $2; exit }'
}

# PIDs of the Fresh CLIENTS attached to daemon $1 (argv exactly `fresh -a NAME`).
fresh_client_pids() {
  local pid
  for pid in $(pgrep -f -- "-a $1" 2>/dev/null); do
    fresh_argv_is "$pid" -a "$1" && printf '%s\n' "$pid"
  done
  return 0
}

# "STATE PGID" of pid $1 from /proc/<pid>/stat, nothing when it is gone. The
# comm field may hold spaces and parentheses, so fields are read after the
# LAST ')'. STATE is R/S/D/Z, or T while stopped.
proc_fields() {
  local stat rest state pgrp
  stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 0
  rest=${stat##*) }
  read -r state _ pgrp _ <<<"$rest"
  printf '%s %s\n' "$state" "$pgrp"
}

# Pause daemon $1 (its clients, then its server's process group). A stopped
# client just waits. The server group also holds the language servers, so they
# stop too and use no CPU. Only a group leader (pgid == pid) gets a group
# signal; anything else gets the pid alone. Never fails.
fresh_pause() { # name
  local pid pgid
  for pid in $(fresh_client_pids "$1"); do kill -STOP "$pid" 2>/dev/null || true; done
  pid=$(fresh_server_pid "$1") || true
  [ -n "$pid" ] || return 0
  read -r _ pgid <<<"$(proc_fields "$pid")"
  if [ "${pgid:-}" = "$pid" ]; then
    kill -STOP -- "-$pid" 2>/dev/null || true
  else
    kill -STOP "$pid" 2>/dev/null || true
  fi
  return 0
}

# Resume daemon $1: the server (its group first) and then its clients. The
# reverse of fresh_pause, so a client never wakes before its server. A cheap
# no-op when nothing is stopped. Never fails.
fresh_resume() { # name
  local pid pgid
  pid=$(fresh_server_pid "$1") || true
  if [ -n "$pid" ]; then
    read -r _ pgid <<<"$(proc_fields "$pid")"
    if [ "${pgid:-}" = "$pid" ]; then
      kill -CONT -- "-$pid" 2>/dev/null || true
    else
      kill -CONT "$pid" 2>/dev/null || true
    fi
  fi
  for pid in $(fresh_client_pids "$1"); do kill -CONT "$pid" 2>/dev/null || true; done
  return 0
}

# Where the explicit Agent-column choice lives (first line = the command).
agent_config_file() {
  local dir=''
  dir=$("$HERDR" plugin config-dir dotfiles-ide 2>/dev/null) || dir=''
  [ -n "$dir" ] || dir=${XDG_CONFIG_HOME:-$HOME/.config}/herdr/plugins/config/dotfiles-ide
  printf '%s/agent\n' "$dir"
}

# The IDE's Agent column command. Precedence: the plugin's `agent` file (the
# user's explicit choice), then $IDE_AGENT, then the first agent that is
# actually INSTALLED — pi before claude, because claude is not a given on a new
# machine and typing a missing binary into a pane just leaves a dead shell that
# looks like the IDE failed. Nothing installed → "none": no Agent column.
agent_command() {
  local file candidate
  file=$(agent_config_file)
  if [ -s "$file" ]; then
    head -n1 "$file"
    return 0
  fi
  if [ -n "${IDE_AGENT:-}" ]; then
    printf '%s\n' "$IDE_AGENT"
    return 0
  fi
  # Ordered by preference, not by completeness: pi first (the setup here), then
  # the CLIs the IDE layer's `ide-agent-title` table knows, so a fresh machine
  # with only codebuddy/codex/gemini still gets an Agent column instead of
  # `none`. Add a line here when a new agent CLI ships.
  for candidate in pi claude codex gemini codebuddy cursor-agent opencode aider qwen kimi grok droid; do
    if command -v "$candidate" >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  printf 'none\n'
}

# Daemon for project root $1 in workspace $2. Each workspace gets its own
# editor: the project's daemon, unless a Fresh pane in another workspace
# already uses it, then the first free "<daemon>-sN" (N = 2, 3, ...). A
# workspace keeps the one it already has. Outside herdr (no $2): the
# project's daemon.
ide_daemon_for() { # root ws
  local base panes used mine cand n
  base=$(daemon_name "$1")
  if [ -z "${2:-}" ] || ! panes=$("$HERDR" pane list 2>/dev/null); then
    printf '%s\n' "$base"; return 0
  fi
  used=$(jq -r --arg ws "$2" --arg lbl "$FRESH_LABEL" '.result.panes[]
    | select(.label == $lbl and .workspace_id != $ws) | .tokens.fresh_daemon // empty' <<<"$panes")
  mine=$(jq -r --arg ws "$2" --arg lbl "$FRESH_LABEL" '.result.panes[]
    | select(.label == $lbl and .workspace_id == $ws) | .tokens.fresh_daemon // empty' <<<"$panes")
  while read -r cand; do
    [[ $cand == "$base" || $cand == "$base"-s[0-9]* ]] || continue
    grep -qxF -- "$cand" <<<"$used" || { printf '%s\n' "$cand"; return 0; }
  done <<<"$mine"
  cand=$base n=1
  while grep -qxF -- "$cand" <<<"$used"; do n=$((n + 1)); cand=$base-s$n; done
  printf '%s\n' "$cand"
}

# Remembered sizes are per project (the Fresh daemon name without its -sN suffix).
sizes_key() { printf '%s\n' "${1%-s[0-9]*}"; }

panes_json() {
  if [ -n "${1:-}" ]; then "$HERDR" pane list --workspace "$1"; else "$HERDR" pane list; fi
}

# Print "pane_id<TAB>workspace_id<TAB>daemon<TAB>cwd" for every Fresh pane,
# current tab first, then current workspace, then everything else.
fresh_panes() {
  panes_json "" | jq -r --arg tab "$cur_tab" --arg ws "$cur_ws" --arg lbl "$FRESH_LABEL" '
    [.result.panes[] | select(.label == $lbl)]
    | sort_by(if .tab_id == $tab then 0 elif .workspace_id == $ws then 1 else 2 end)
    | .[] | [.pane_id, .workspace_id, (.tokens.fresh_daemon // ""), (.foreground_cwd // .cwd // "")] | @tsv'
}

# The `fresh_daemon` token of pane $1, or nothing.
pane_fresh_daemon() {
  "$HERDR" pane list | jq -r --arg p "$1" '
    first(.result.panes[] | select(.pane_id == $p) | .tokens.fresh_daemon) // empty'
}

# True when pane $1 is a WORKING Fresh editor: it carries a `fresh_daemon` token
# and that daemon is running. The label alone says nothing: a herdr server
# restart resumes every pane as a plain shell, KEEPING the labels but DROPPING
# the metadata tokens, so a pane labelled `Fresh` can be an empty prompt (its
# `fresh-ide` is gone, and with it the editor). `pane list` marks such a pane
# with no tokens at all.
fresh_editor_ok() { # pane
  local daemon
  daemon=$(pane_fresh_daemon "$1") || daemon=''
  [ -n "$daemon" ] || return 1
  daemon_alive "$daemon"
}

# First pane in tab $1 carrying label $2.
pane_with_label() {
  "$HERDR" pane list | jq -r --arg tab "$1" --arg lbl "$2" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl) | .pane_id) // empty'
}

pane_field() {
  "$HERDR" pane get "$1" | jq -r --arg f "$2" '.result.pane[$f] // empty'
}

tab_zoomed() {
  "$HERDR" pane layout --pane "$1" | jq -r '.result.layout.zoomed'
}

# The herdr CLI can only focus a pane by direction; pane.focus by id is a
# socket method, so it goes through the tiny herdr-rpc helper.
focus_pane() {
  "$(dirname "${BASH_SOURCE[0]}")/herdr-rpc" pane.focus "{\"pane_id\":\"$1\"}" >/dev/null
}

mark_fresh_pane() { # pane daemon
  "$HERDR" pane rename "$1" "$FRESH_LABEL" >/dev/null
  "$HERDR" pane report-metadata "$1" --source "$TOKEN_SOURCE" --token "fresh_daemon=$2" >/dev/null
  # Right-click goes to Fresh's own context menu, not herdr's pane menu
  # (right-clicking the pane border still opens herdr's).
  "$HERDR" pane input "$1" --right-click pane >/dev/null
}

# New editor pane above the terminal pane $1, rooted at $2, taking $3 of the
# column (the editor/terminal SPLIT RATIO, i.e. the EDITOR's share): split $1
# down, then swap so the new pane sits on top. Prints its id; the caller marks
# and starts it. Callers compute $3 with editor_ratio_for (a remembered ratio
# wins over the sidebar's terminal-height setting). Used by herdr-ide and
# fresh-restart (pane gone case).
new_editor_pane() { # term root ratio
  local editor ratio=${3:-0.7}
  [ -n "$ratio" ] || ratio=0.7
  editor=$("$HERDR" pane split "$1" --direction down --ratio "$ratio" --cwd "$2" \
    | jq -r '.result.pane.pane_id // empty')
  [ -n "$editor" ] || return 1
  "$HERDR" pane swap --source-pane "$1" --target-pane "$editor" >/dev/null
  printf '%s\n' "$editor"
}

# ── Editor/terminal split (terminal height) ────────────────────────────
# The split ratio is the TOP (editor) child's share, so a terminal of N rows
# out of a column of H rows is 1 - N/H. Three sources, in order:
#   1. sizes.json's remembered "editor" ratio for this project (the user's own
#      drag ALWAYS wins),
#   2. the herdr-sidebar plugin's `terminal_height` setting (rows) — the ONE-WAY
#      dependency: this layer only READS the sidebar's state file,
#   3. the historical 0.7.
# The sidebar's "Terminal height" row also applies itself to the tab it runs in;
# these helpers only matter when a layout is (re)built.
SIDEBAR_STATE_FILE=${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/herdr-sidebar/state.json
# The sidebar stamps the moment it starts (re)docking into a tab, in its own
# runtime dir. A re-dock reaches a full-height column by moving panes through a
# temporary tab, and herdr re-creates every split that touches at a DEFAULT
# ratio — so a sample taken meanwhile is an artifact of the dock, not a choice
# the user made.
SIDEBAR_DOCK_STAMP=${SIDEBAR_STATE_FILE%/*}/dock/restore.stamp
# How long a dock stamp suppresses size sampling: the repair itself plus the
# focus events that trail it (the dock's own pane.focused / tab.focused).
DOCK_SUPPRESS_SECS=20
# Historical editor share; also the fallback inside editor_split_ratio.
DEFAULT_EDITOR_RATIO=0.7
# Historical Agent-column split ratio; also the fallback in agent_split_ratio.
DEFAULT_AGENT_RATIO=0.6

# One numeric field of the sidebar's state file, or nothing when the file is
# missing, unreadable, or the key is absent/null. Never fails.
sidebar_state_number() { # key
  local value
  [ -r "$SIDEBAR_STATE_FILE" ] || return 0
  # Numbers only: the sidebar writes a number or null, and a hand-edited
  # `"10"` must read as unset exactly like the Rust parser (`as_u64`).
  value=$(jq -r --arg k "$1" '.[$k] | numbers' "$SIDEBAR_STATE_FILE" 2>/dev/null) || value=''
  case $value in
    '' | *[!0-9]*) return 0 ;;
  esac
  printf '%s\n' "$value"
}

# The sidebar's Terminal height setting in ROWS, or nothing when unset ("auto")
# or unreadable.
terminal_height_rows() {
  local rows
  rows=$(sidebar_state_number terminal_height) || rows=''
  [ -n "$rows" ] || return 0
  # Same bounds as the sidebar's row (4..40 rows, 2 per step).
  if [ "$rows" -lt 4 ]; then rows=4; elif [ "$rows" -gt 40 ]; then rows=40; fi
  printf '%s\n' "$rows"
}

# The sidebar's Agent width setting in COLUMNS, or nothing when unset ("auto")
# or unreadable.
agent_width_cols() {
  local cols
  cols=$(sidebar_state_number agent_width) || cols=''
  [ -n "$cols" ] || return 0
  # Same bounds as the sidebar's row (24..320 columns, 4 per step).
  if [ "$cols" -lt 24 ]; then cols=24; elif [ "$cols" -gt 320 ]; then cols=320; fi
  printf '%s\n' "$cols"
}

# The editor (top) share for a terminal pane about to be split in tab layout
# $1, pane $2. A remembered ratio ($3, sizes.json "editor") always wins — the
# user's own drag — otherwise the sidebar's `terminal_height` setting (rows),
# otherwise the historical 0.7. Pure: takes a `pane layout` reply on stdin and
# calls no herdr command, so it is unit-testable on fixtures.
editor_split_ratio() { # pane saved-ratio [fallback] < layout.json
  local pane=$1 saved=${2:-} fallback=${3:-$DEFAULT_EDITOR_RATIO} rows height millionths
  if [ -n "$saved" ]; then
    printf '%.6f\n' "$saved"
    return 0
  fi
  rows=$(terminal_height_rows)
  if [ -z "$rows" ]; then
    printf '%.6f\n' "$fallback"
    return 0
  fi
  height=$(jq -r --arg pane "$pane" '
    .result.layout as $L
    | (first($L.panes[]? | select(.pane_id == $pane) | .rect) // null) as $r
    | if $r == null then empty
      else
        # The COLUMN height. Before the split exists the pane spans it, but a
        # collapsed terminal leaves the editor short of the column, so prefer
        # the smallest enclosing `down` split.
        ([ $L.splits[]? | select(.direction == "down")
           | select(.rect.x <= $r.x and $r.x + $r.width <= .rect.x + .rect.width
                    and .rect.y <= $r.y and $r.y + $r.height <= .rect.y + .rect.height)
           | .rect.height ] | min) // $r.height
      end' 2>/dev/null) || height=''
  case $height in
    '' | *[!0-9]*) printf '%.6f\n' "$fallback"; return 0 ;;
  esac
  # Both panes keep at least one row; herdr clamps the ratio to 0.1..0.9.
  if [ "$height" -lt 3 ]; then
    printf '%.6f\n' "$fallback"
    return 0
  fi
  if [ "$rows" -lt 1 ]; then rows=1; elif [ "$rows" -gt $((height - 1)) ]; then rows=$((height - 1)); fi
  # Integer math in millionths: (height - rows) / height.
  millionths=$(((height - rows) * 1000000 / height))
  if [ "$millionths" -lt 100000 ]; then millionths=100000; fi
  if [ "$millionths" -gt 900000 ]; then millionths=900000; fi
  printf '%d.%06d\n' $((millionths / 1000000)) $((millionths % 1000000))
}

# editor_split_ratio for the layout being built around pane $1, with the tab's
# saved sizes entry $2 ({"editor": r, “agent”: r}, or empty) supplying the
# remembered ratio. Takes one `pane layout` snapshot.
editor_ratio_for() { # pane saved-entry
  local entry=${2:-} saved
  [ -n "$entry" ] || entry='{}'
  saved=$(jq -r '.editor // empty' <<<"$entry" 2>/dev/null) || saved=''
  editor_split_ratio "$1" "$saved" <<<"$("$HERDR" pane layout --pane "$1" 2>/dev/null || true)"
}

# ── Agent column split (agent width) ───────────────────────────────────
# The split ratio at both herdr-ide build sites is the LEFT (middle) child's
# share of a `right` split, so an Agent column of N columns out of a W-wide
# anchor is 1 - N/W — the mirror of editor_split_ratio's 1 - rows/H. Three
# sources, in order:
#   1. sizes.json's remembered "agent" ratio for this project (the user's own
#      drag ALWAYS wins),
#   2. the herdr-sidebar plugin's `agent_width` setting (columns),
#   3. the historical 0.6.

# The Agent-column split ratio for the anchor pane $1 about to be split
# rightwards in the layout on stdin. A remembered ratio ($2, sizes.json
# "agent") wins, otherwise the sidebar's `agent_width` setting, otherwise $3.
# Pure: takes a `pane layout` reply on stdin and calls no herdr command, so it
# is unit-testable on fixtures.
agent_split_ratio() { # pane saved-ratio [fallback] < layout.json
  local pane=$1 saved=${2:-} fallback=${3:-$DEFAULT_AGENT_RATIO} cols width millionths
  if [ -n "$saved" ]; then
    printf '%.6f\n' "$saved"
    return 0
  fi
  cols=$(agent_width_cols)
  if [ -z "$cols" ]; then
    printf '%.6f\n' "$fallback"
    return 0
  fi
  # The anchor pane spans the width being split, so its own rect width IS W.
  width=$(jq -r --arg pane "$pane" '
    first(.result.layout.panes[]? | select(.pane_id == $pane) | .rect.width) // empty' \
    2>/dev/null) || width=''
  case $width in
    '' | *[!0-9]*) printf '%.6f\n' "$fallback"; return 0 ;;
  esac
  # Both panes keep at least one column; herdr clamps the ratio to 0.1..0.9.
  if [ "$width" -lt 3 ]; then
    printf '%.6f\n' "$fallback"
    return 0
  fi
  if [ "$cols" -lt 1 ]; then cols=1; elif [ "$cols" -gt $((width - 1)) ]; then cols=$((width - 1)); fi
  # Integer math in millionths: (width - cols) / width.
  millionths=$(((width - cols) * 1000000 / width))
  if [ "$millionths" -lt 100000 ]; then millionths=100000; fi
  if [ "$millionths" -gt 900000 ]; then millionths=900000; fi
  printf '%d.%06d\n' $((millionths / 1000000)) $((millionths % 1000000))
}

# agent_split_ratio for the layout being built around pane $1, with the tab's
# saved sizes entry $2 supplying the remembered ratio. Takes one `pane layout`
# snapshot.
agent_ratio_for() { # pane saved-entry
  local entry=${2:-} saved
  [ -n "$entry" ] || entry='{}'
  saved=$(jq -r '.agent // empty' <<<"$entry" 2>/dev/null) || saved=''
  agent_split_ratio "$1" "$saved" <<<"$("$HERDR" pane layout --pane "$1" 2>/dev/null || true)"
}

# ── Remembered sizes ───────────────────────────────────────────────────
# The Agent and editor/terminal split ratios are kept per project (the Fresh
# daemon name without its -sN suffix) in $STATE/sizes.json, so a rebuilt layout comes back the way
# the user left it. Splits are found by geometry, never by herdr's ids.

STATE=${HERDR_PLUGIN_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/dotfiles-ide}
SIZES_FILE=$STATE/sizes.json
SIZES_LOCK=$STATE/sizes.lock
# herdr-ide-term collapses the terminal to a strip of at most this many rows;
# a ratio measured then is the collapsed one, not the user's, so never save it.
TERM_COLLAPSED_ROWS=5

# Token of the Fresh pane in tab $1 (the project's daemon name), if any.
tab_fresh_daemon() {
  "$HERDR" pane list | jq -r --arg tab "$1" --arg lbl "$FRESH_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl
      and (.tokens.fresh_daemon // "") != "") | .tokens.fresh_daemon) // empty'
}

# True while herdr-sidebar is (re)docking into this machine's tabs — see
# SIDEBAR_DOCK_STAMP. A missing, empty, beyond-DOCK_SUPPRESS_SECS, or garbled
# stamp is false: never suppress a save on a guess, and never let one live
# marker disable saving forever. A stamp from the FUTURE is a clock change, not
# a live dock. Pure apart from one `date` call; never fails.
recent_dock_reorder() {
  local stamp now
  [ -r "$SIDEBAR_DOCK_STAMP" ] || return 1
  read -r stamp <"$SIDEBAR_DOCK_STAMP" 2>/dev/null || return 1
  case $stamp in
    '' | *[!0-9]*) return 1 ;;
  esac
  now=$(date +%s 2>/dev/null) || return 1
  case $now in
    '' | *[!0-9]*) return 1 ;;
  esac
  [ "$stamp" -le "$now" ] && [ $((now - stamp)) -le "$DOCK_SUPPRESS_SECS" ]
}

# Read a layout (`pane layout` reply) on stdin and print the two IDE splits as
# {"agent": {"path": [...], "ratio": r}, "editor": {...}}, omitting any that
# is not there. Paths are the layout ids decoded: split_1_01 -> [false,true].
#   agent  = a right split whose boundary sits on the Agent pane's left edge
#   editor = a down split holding Fresh and Terminal whose boundary sits on
#            the Terminal's top edge
# usage: ide_splits AGENT_ID FRESH_ID TERM_ID < layout.json
ide_splits() {
  jq -c --arg agent "$1" --arg fresh "$2" --arg term "$3" '
    .result.layout as $L
    | def rect_of($id): first($L.panes[] | select(.pane_id == $id) | .rect) // null;
      def inside($r; $p): $p.x >= $r.x and $p.x < $r.x + $r.width
        and $p.y >= $r.y and $p.y < $r.y + $r.height;
      def spath: .id | sub("^split_[0-9]+_"; "")
        | if . == "root" then [] else [split("")[] | . == "1"] end;
      (rect_of($agent)) as $a
      | (rect_of($fresh)) as $f
      | (rect_of($term)) as $t
      | (if $a then (first($L.splits[] | select(.direction == "right")
            | select(inside(.rect; $a))
            | select(((.rect.x + (.rect.width * .ratio | round)) - $a.x | fabs) <= 1)) // null) else null end) as $s
      | (if $f and $t then (first($L.splits[] | select(.direction == "down")
            | select(inside(.rect; $f) and inside(.rect; $t))
            | select(((.rect.y + (.rect.height * .ratio | round)) - $t.y | fabs) <= 1)) // null) else null end) as $e
      | (if $s then {agent: {path: ($s | spath), ratio: $s.ratio}} else {} end)
        + (if $e then {editor: {path: ($e | spath), ratio: $e.ratio}} else {} end)'
}

# The saved entry {agent, editor} for daemon $1, or {}.
saved_entry() {
  jq -c --arg k "$(sizes_key "$1")" '.[$k] // {}' "$SIZES_FILE" 2>/dev/null || printf '{}\n'
}

# Record the sizes of tab $1 under its project's daemon. $2 is an optional
# pane list (`pane list` reply) so bulk callers take one snapshot.
# Silent no-op when the tab has no tokened Fresh pane or nothing to record.
save_sizes() {
  local tab=$1 panes=${2:-} fresh daemon term agent layout splits term_rows new
  # A herdr-sidebar re-dock is in flight (or just finished): its full-height
  # repair rebuilds this tab's splits, and herdr gives each re-created split a
  # DEFAULT ratio. Sampling now would record that intermediate ratio as the
  # user's own and lose the real one for good, so skip the WHOLE pass (both
  # fields, like the empty-entry early-out below). The stamp covers the repair
  # and the focus events it emits; the next real focus change saves again.
  recent_dock_reorder && return 0
  [ -n "$panes" ] || panes=$("$HERDR" pane list)
  read -r fresh daemon < <(jq -r --arg tab "$tab" --arg lbl "$FRESH_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl
      and (.tokens.fresh_daemon // "") != "") | "\(.pane_id) \(.tokens.fresh_daemon)") // " "' <<<"$panes")
  [ -n "$fresh" ] && [ -n "$daemon" ] || return 0
  term=$(jq -r --arg tab "$tab" --arg lbl "$TERM_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl) | .pane_id) // empty' <<<"$panes")
  agent=$(jq -r --arg tab "$tab" --arg lbl "$AGENT_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl) | .pane_id) // empty' <<<"$panes")
  layout=$("$HERDR" pane layout --pane "$fresh") || return 0
  splits=$(ide_splits "$agent" "$fresh" "$term" <<<"$layout") || return 0
  term_rows=$(jq -r --arg t "$term" '.result.layout.panes[] | select(.pane_id == $t) | .rect.height' <<<"$layout" | head -n1) || true
  # A collapsed terminal says nothing about the editor split the user chose.
  if [ -z "$term" ] || [ -z "$term_rows" ] || [ "$term_rows" -le "$TERM_COLLAPSED_ROWS" ]; then
    splits=$(jq -c 'del(.editor)' <<<"$splits")
  fi
  new=$(jq -c '{agent: .agent.ratio, editor: .editor.ratio} | with_entries(select(.value != null))' <<<"$splits") || return 0
  [ "$new" != '{}' ] || return 0
  mkdir -p "$STATE"
  (
    flock 9
    local src tmp
    src=$(cat "$SIZES_FILE" 2>/dev/null || printf '{}')
    tmp=$(mktemp "$SIZES_FILE.XXXXXX") || exit 1
    if jq --arg k "$(sizes_key "$daemon")" --argjson new "$new" '.[$k] = ((.[$k] // {}) + $new)' <<<"$src" >"$tmp" 2>/dev/null; then
      mv -f "$tmp" "$SIZES_FILE"
    else
      rm -f "$tmp"
    fi
  ) 9>"$SIZES_LOCK" || true
}

# Apply the saved ratios to tab $1's IDE splits. $2 is an entry already read
# (callers that rebuild the layout read it first: the hooks save while a
# build is still in progress). Splits that are missing are skipped.
restore_sizes() {
  local tab=$1 entry=${2:-} panes fresh daemon agent term layout splits kind path ratio
  panes=$("$HERDR" pane list)
  read -r fresh daemon < <(jq -r --arg tab "$tab" --arg lbl "$FRESH_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl
      and (.tokens.fresh_daemon // "") != "") | "\(.pane_id) \(.tokens.fresh_daemon)") // " "' <<<"$panes")
  [ -n "$fresh" ] && [ -n "$daemon" ] || return 0
  [ -n "$entry" ] || entry=$(saved_entry "$daemon")
  [ "$entry" != '{}' ] || return 0
  term=$(jq -r --arg tab "$tab" --arg lbl "$TERM_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl) | .pane_id) // empty' <<<"$panes")
  agent=$(jq -r --arg tab "$tab" --arg lbl "$AGENT_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $lbl) | .pane_id) // empty' <<<"$panes")
  layout=$("$HERDR" pane layout --pane "$fresh") || return 0
  splits=$(ide_splits "$agent" "$fresh" "$term" <<<"$layout") || return 0
  for kind in agent editor; do
    ratio=$(jq -r --arg k "$kind" '.[$k] // empty' <<<"$entry")
    path=$(jq -c --arg k "$kind" '.[$k].path // empty' <<<"$splits")
    [ -n "$ratio" ] && [ -n "$path" ] || continue
    "$(dirname "${BASH_SOURCE[0]}")/herdr-rpc" layout.set_split_ratio \
      "{\"tab_id\":\"$tab\",\"path\":$path,\"ratio\":$ratio}" >/dev/null || true
  done
}

# Saved editor ratio for the project of Fresh pane $1, or $2 when none is saved
# and the sidebar has no Terminal height setting.
saved_editor_ratio() { # fresh-pane fallback
  local daemon ratio
  daemon=$("$HERDR" pane list | jq -r --arg p "$1" '
    first(.result.panes[] | select(.pane_id == $p) | .tokens.fresh_daemon) // empty')
  ratio=$(jq -r '.editor // empty' <<<"$(saved_entry "$daemon")" 2>/dev/null) || ratio=''
  editor_split_ratio "$1" "$ratio" "$2" \
    <<<"$("$HERDR" pane layout --pane "$1" 2>/dev/null || true)"
}

# ── Toasts naming a shortcut ───────────────────────────────────────────

# Print the key bound to plugin action $1 in herdr's config, pretty-printed
# ("prefix+i" -> "Ctrl+B I"), or nothing when no binding exists.
keybinding_for() {
  local cfg=${HERDR_CONFIG_PATH:-${XDG_CONFIG_HOME:-$HOME/.config}/herdr/config.toml}
  [ -r "$cfg" ] || return 0
  python3 -I -c '
import sys, tomllib
action, path = sys.argv[1], sys.argv[2]
with open(path, "rb") as f:
    cfg = tomllib.load(f)
keys = cfg.get("keys", {})
prefix = keys.get("prefix", "ctrl+b")
names = {"ctrl": "Ctrl", "alt": "Alt", "shift": "Shift", "cmd": "Cmd",
         "backtick": "`", "enter": "Enter", "esc": "Esc", "tab": "Tab", "space": "Space"}
def chord(s):
    out = []
    for part in s.split("+"):
        if part in names:
            out.append(names[part])
        elif len(part) == 1:
            out.append(part.upper())
        else:
            out.append(part.capitalize())
    return "+".join(out)
for entry in keys.get("command", []):
    if entry.get("command") == action:
        key = entry.get("key", "")
        if key.startswith("prefix+"):
            print(chord(prefix) + " " + chord(key[len("prefix+"):]))
        else:
            print(chord(key))
        break
' "$1" "$cfg" 2>/dev/null || true
}

# tip TITLE ACTION — toast TITLE with the shortcut bound to ACTION as its body.
# Never fails and never blocks long; callers only use it for action runs.
tip() {
  local body
  body=$(keybinding_for "$2")
  if [ -n "$body" ]; then
    timeout 3 "$HERDR" notification show "$1" --body "$body" >/dev/null 2>&1 || true
  else
    timeout 3 "$HERDR" notification show "$1" >/dev/null 2>&1 || true
  fi
}
