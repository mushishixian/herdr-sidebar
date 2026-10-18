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
  used=$(jq -r --arg ws "$2" --arg label "$FRESH_LABEL" '.result.panes[]
    | select(.label == $label and .workspace_id != $ws) | .tokens.fresh_daemon // empty' <<<"$panes")
  mine=$(jq -r --arg ws "$2" --arg label "$FRESH_LABEL" '.result.panes[]
    | select(.label == $label and .workspace_id == $ws) | .tokens.fresh_daemon // empty' <<<"$panes")
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
  panes_json "" | jq -r --arg tab "$cur_tab" --arg ws "$cur_ws" --arg label "$FRESH_LABEL" '
    [.result.panes[] | select(.label == $label)]
    | sort_by(if .tab_id == $tab then 0 elif .workspace_id == $ws then 1 else 2 end)
    | .[] | [.pane_id, .workspace_id, (.tokens.fresh_daemon // ""), (.foreground_cwd // .cwd // "")] | @tsv'
}

# First pane in tab $1 carrying label $2.
pane_with_label() {
  "$HERDR" pane list | jq -r --arg tab "$1" --arg label "$2" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label) | .pane_id) // empty'
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
# Historical editor share; also the fallback inside editor_split_ratio.
DEFAULT_EDITOR_RATIO=0.7

# The sidebar's Terminal height setting in ROWS, or nothing when unset ("auto")
# or unreadable. Never fails: a missing/garbled file simply means "auto".
terminal_height_rows() {
  local value
  [ -r "$SIDEBAR_STATE_FILE" ] || return 0
  # Numbers only: the sidebar writes a number or null, and a hand-edited
  # `"10"` must read as "auto" exactly like the Rust parser (`as_u64`).
  value=$(jq -r '.terminal_height | numbers' "$SIDEBAR_STATE_FILE" 2>/dev/null) || value=''
  case $value in
    '' | *[!0-9]*) return 0 ;;
  esac
  # Same bounds as the sidebar's row (4..40 rows, 2 per step).
  if [ "$value" -lt 4 ]; then value=4; elif [ "$value" -gt 40 ]; then value=40; fi
  printf '%s\n' "$value"
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
  "$HERDR" pane list | jq -r --arg tab "$1" --arg label "$FRESH_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label
      and (.tokens.fresh_daemon // "") != "") | .tokens.fresh_daemon) // empty'
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
  [ -n "$panes" ] || panes=$("$HERDR" pane list)
  read -r fresh daemon < <(jq -r --arg tab "$tab" --arg label "$FRESH_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label
      and (.tokens.fresh_daemon // "") != "") | "\(.pane_id) \(.tokens.fresh_daemon)") // " "' <<<"$panes")
  [ -n "$fresh" ] && [ -n "$daemon" ] || return 0
  term=$(jq -r --arg tab "$tab" --arg label "$TERM_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label) | .pane_id) // empty' <<<"$panes")
  agent=$(jq -r --arg tab "$tab" --arg label "$AGENT_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label) | .pane_id) // empty' <<<"$panes")
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
  read -r fresh daemon < <(jq -r --arg tab "$tab" --arg label "$FRESH_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label
      and (.tokens.fresh_daemon // "") != "") | "\(.pane_id) \(.tokens.fresh_daemon)") // " "' <<<"$panes")
  [ -n "$fresh" ] && [ -n "$daemon" ] || return 0
  [ -n "$entry" ] || entry=$(saved_entry "$daemon")
  [ "$entry" != '{}' ] || return 0
  term=$(jq -r --arg tab "$tab" --arg label "$TERM_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label) | .pane_id) // empty' <<<"$panes")
  agent=$(jq -r --arg tab "$tab" --arg label "$AGENT_LABEL" '
    first(.result.panes[] | select(.tab_id == $tab and .label == $label) | .pane_id) // empty' <<<"$panes")
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
