#!/usr/bin/env bash
# herdr-agent-state.sh — report CodeBuddy's agent state to herdr.
#
# herdr ships integrations for 18 coding agents (herdr integration status); it
# has NO CodeBuddy target, so `herdr integration install codebuddy` does not
# exist and herdr never detects the `codebuddy` process at all. CodeBuddy's own
# hook system (Claude-Code compatible) is therefore the carrier: installing this
# script in ~/.codebuddy/settings.json's `hooks` map makes the pane's herdr
# identity say `idle` / `working` / `blocked`.
#
# Installed by dotfiles/install.sh (the `codebuddy agent-state hook` step),
# which links this file to ~/.codebuddy/hooks/ and merges the hook entries into
# the user's settings — only entries whose command ends in this file's name are
# replaced, so model/permissions/plugins survive every rerun.
#
# Contract (verified live against herdr 0.9.5):
#   * `pane report-agent` needs the pane to actually HOST an agent process: the
#     same call on a pane holding a bare shell is answered {"type":"ok"} and
#     silently dropped, and `pane.list` keeps `agent: null`. A hook always runs
#     while codebuddy is the pane's foreground process, so that holds here.
#   * `--source` must NOT start with `herdr:` — that prefix belongs to herdr's
#     own integrations and a report using it is dropped.
#   * `--seq` must be strictly greater than the last one herdr accepted from this
#     source, forever (a future-dated seq would even survive an agent restart
#     and block it), so it is a monotonic nanosecond stamp persisted per pane.
#   * Blocking is invisible: print nothing, always exit 0. Anything on stdout is
#     fed back into the model's context, and exit 2 blocks the tool call.
#
# Event → state (CodeBuddy hook names, Claude-Code compatible):
#   SessionStart      → idle    (+ the session ref, so herdr can resume it)
#   UserPromptSubmit  → working  (a turn started)
#   PreToolUse        → working  (also clears a blocked state once the user
#                                 approves a permission prompt and the tool runs)
#   Stop              → idle     (the reply finished and CodeBuddy waits for you)
#   SubagentStop      → ignored  (the main agent keeps working)
#   PermissionRequest → blocked  (a decision is required)
#   Notification      → blocked for permission_prompt / agent_needs_input /
#                       elicitation_dialog; idle_prompt means "waiting for you",
#                       which is idle; background chatter is ignored.
#   SessionEnd        → release  (name and state come off the pane at once)
#
# Nothing here is allowed to fail loudly: a hook that errors or hangs would slow
# the agent down, and every herdr call is bounded by `timeout 3`.
set -uo pipefail

# Outside herdr there is nothing to report to (HERDR_ENV is injected into every
# pane's environment; HERDR_SOCKET_PATH is what the CLI actually talks to).
[ "${HERDR_ENV:-}" = 1 ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v "${HERDR_BIN_PATH:-herdr}" >/dev/null 2>&1 || exit 0

SOURCE=codebuddy
AGENT=codebuddy
herdr=${HERDR_BIN_PATH:-herdr}

# One read: the hook payload is a single JSON object on stdin.
payload=$(cat 2>/dev/null) || payload=''
[ -n "$payload" ] || exit 0

# field NAME — a top-level string field of the payload, or empty. A payload that
# is not JSON (or a machine without jq, which the dotfiles installer requires
# anyway) makes this silent instead of fatal.
field() {
  local value
  command -v jq >/dev/null 2>&1 || return 0
  value=$(printf '%s' "$payload" | jq -r --arg k "$1" \
    'if type == "object" then (.[$k] // "" | tostring) else "" end' 2>/dev/null) || value=''
  printf '%s' "$value"
}

event=$(field hook_event_name)
[ -n "$event" ] || exit 0
session_id=$(field session_id)
transcript=$(field transcript_path)

# ── Monotonic seq ──────────────────────────────────────────────────────
# herdr drops any report whose seq is not higher than the last one it accepted
# from this source, and a seq from the FUTURE is not corrected by an agent
# restart — it just blocks every later report. So the value only ever has to be
# greater than the previous one: now (nanoseconds) or last + 1, whichever wins.
seq_dir=${XDG_STATE_HOME:-$HOME/.local/state}/herdr-codebuddy
seq_file=$seq_dir/seq-${HERDR_PANE_ID//[^A-Za-z0-9._-]/_}
mkdir -p "$seq_dir" 2>/dev/null || exit 0

next_seq() {
  local now last=0
  now=$(date +%s%N 2>/dev/null) || now=''
  case $now in
    '' | *[!0-9]*) now=$(($(date +%s) * 1000000000)) ;;
  esac
  if [ -r "$seq_file" ]; then
    read -r last <"$seq_file" 2>/dev/null || last=0
  fi
  case ${last:-0} in
    '' | *[!0-9]*) last=0 ;;
  esac
  if [ "$now" -le "$last" ]; then now=$((last + 1)); fi
  printf '%s\n' "$now" >"$seq_file" 2>/dev/null || true
  printf '%s\n' "$now"
}

# report STATE [MESSAGE] — one `pane report-agent`. The session reference rides
# along on every report (harmless, and it is what herdr restores the pane with
# after a server restart).
report() {
  local state=$1 seq
  local -a args=(
    pane report-agent "$HERDR_PANE_ID"
    --source "$SOURCE" --agent "$AGENT" --state "$state"
  )
  seq=$(next_seq) || return 0
  args+=(--seq "$seq")
  [ -n "${2:-}" ] && args+=(--message "$2")
  [ -n "$session_id" ] && args+=(--agent-session-id "$session_id")
  case $transcript in
    /*) args+=(--agent-session-path "$transcript") ;;
  esac
  timeout 3 "$herdr" "${args[@]}" >/dev/null 2>&1 || true
  return 0
}

# release — give the pane back when the agent really quits (herdr otherwise
# waits for the pane to return to its shell prompt before clearing the name).
release() {
  local seq
  seq=$(next_seq) || return 0
  timeout 3 "$herdr" pane release-agent "$HERDR_PANE_ID" \
    --source "$SOURCE" --agent "$AGENT" --seq "$seq" >/dev/null 2>&1 || true
  return 0
}

NEEDS_INPUT='CodeBuddy needs a decision'

case $event in
  SessionStart) report idle ;;
  UserPromptSubmit | PreToolUse) report working ;;
  Stop) report idle ;;
  PermissionRequest) report blocked "$NEEDS_INPUT" ;;
  Notification)
    case $(field notification_type) in
      permission_prompt | agent_needs_input | elicitation_dialog)
        report blocked "$NEEDS_INPUT"
        ;;
      idle_prompt) report idle ;;
      *) ;;
    esac
    ;;
  SessionEnd) release ;;
  *) ;;
esac
exit 0
