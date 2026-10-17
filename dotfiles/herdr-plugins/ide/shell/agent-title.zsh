# agent-title.zsh — source this in the IDE's Agent pane (zsh only). After each
# command the helper titles the pane by the agent it runs, and clears the title
# at the next prompt (see bin/ide-agent-title). Sourcing twice is harmless.
#   . ~/code/herdr-sidebar/dotfiles/herdr-plugins/ide/shell/agent-title.zsh
[[ -n ${ZSH_VERSION:-} && -n ${HERDR_PANE_ID:-} ]] || return 0
(( ${+_ide_agent_title_loaded} )) && return 0
typeset -g _ide_agent_title_loaded=1
_ide_agent_helper=${${(%):-%x}:A:h}/../bin/ide-agent-title

autoload -Uz add-zsh-hook

_ide_agent_preexec() {
  _ide_agent_ran=1
  "$_ide_agent_helper" "$HERDR_PANE_ID" "$1" >/dev/null 2>&1 &!
}

_ide_agent_precmd() {
  if [[ -n ${_ide_agent_ran:-} ]]; then
    unset _ide_agent_ran
    "$_ide_agent_helper" --clear "$HERDR_PANE_ID" >/dev/null 2>&1 &!
  fi
}

add-zsh-hook preexec _ide_agent_preexec
add-zsh-hook precmd _ide_agent_precmd

# The command that sources this file already started, so the first prompt must clear.
_ide_agent_ran=1
