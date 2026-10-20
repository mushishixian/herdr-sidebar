#!/usr/bin/env bash
# 把一个 path[:line[:col]] 推进「这个项目所在 workspace」的 Fresh 编辑器 pane，
# 并把那个 pane 带到前台。被 jump-file.sh / jump-symbol.sh 调用；也可单独手动用：
#   bash ~/.config/herdr/scripts/open-in-fresh.sh /data/www/xxx/app/Models/User.php:42
#
# 会话命名跟 herdr-fresh 插件同一套：fresh-<herdr workspace_id>，已有会话直接续用。
# Fresh 还没开时，借 herdr-fresh 插件的 pane entrypoint 把 Fresh 拉起来——fresh 起
# 会话必须有真 PTY（headless 里 `fresh -a` 起不来，见插件 manifest 的踩坑记录），
# 所以这里只负责开 pane，会话由 pane 里的 fresh 自己注册。
#
# 实测结论（fresh 0.2.12 + herdr 0.9.5）：
#   * `herdr plugin pane open` 会用 herdr 自己算的 context 覆盖 --env 注入的
#     HERDR_PLUGIN_CONTEXT_JSON，且不带 --workspace 时 pane 落在「全局焦点所在的
#     workspace」——所以必须显式 --workspace，然后认领新出现的那个 fresh-* 会话，
#     不要假设它叫什么名字。
#   * 千万别碰 `fresh --cmd session info`：它会把还活着的会话当 stale 清掉。
#     只用 session list / session open-file。
#   * `plugin pane open --placement split --target-pane` 在 0.9.5 上不认 pane id（报
#     invalid_params: use target_pane_id），所以只能先按 tab 把 Fresh 开出来，再用
#     `pane move` 搬进你按键所在的那个 tab。不搬的话 Fresh 独占一个 tab，一推文件
#     视图就被切走，旁边的 agent 看着像「消失了」——它其实一直活着。
set -uo pipefail

export PATH="$HOME/.local/bin:$HOME/.pi/agent/bin:$PATH"
# fresh 0.4+ 把 `--cmd session ...` 改名成 `--cmd daemon ...`（0.5.3 里 session 还是别名，
# 但别赌它一直在）。先试 daemon，不行退回 session，两个版本都能跑。
fresh_group() {
  if fresh --cmd daemon list >/dev/null 2>&1; then printf 'daemon'; else printf 'session'; fi
}
FRESH_GROUP="$(fresh_group)"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
target="${1:?usage: open-in-fresh.sh <path[:line[:col]]>}"

# ---- 当前是哪个 workspace / pane / tab（键在哪儿按的，Fresh 就住哪儿）----
cur=$("$herdr_bin" pane current 2>/dev/null || true)
# HOF_INVOKE_* 是给事件钩子用的：钩子不在任何 pane 里跑，pane current 指向的是它正要
# 关掉的那个 tab，所以要允许外部显式告诉我们「键是在哪个 tab/pane 按的」。
ws="${HOF_INVOKE_WS:-$(printf '%s' "$cur" | jq -r '.result.pane.workspace_id // empty' 2>/dev/null || true)}"
invoke_pane="${HOF_INVOKE_PANE:-$(printf '%s' "$cur" | jq -r '.result.pane.pane_id // empty' 2>/dev/null || true)}"
invoke_tab="${HOF_INVOKE_TAB:-$(printf '%s' "$cur" | jq -r '.result.pane.tab_id // empty' 2>/dev/null || true)}"
if [ -z "$ws" ]; then
  printf '\033[1;31m定位不到当前 workspace（herdr pane current 没给 workspace_id）\033[0m\n'
  printf '\n按回车关闭…'
  read -r _ || true
  exit 1
fi
name="fresh-$ws"

# ---- 定位 Fresh pane：它自己会把 label 改成会话名 ----
fresh_pane() {
  "$herdr_bin" pane list 2>/dev/null | jq -r --arg n "$name" '
    (.result.panes // []) | map(select(.label == $n)) | (.[0].pane_id // empty)' 2>/dev/null || true
}
pane_tab() {
  "$herdr_bin" pane list 2>/dev/null | jq -r --arg p "${1:-}" '
    (.result.panes // []) | map(select(.pane_id == $p)) | (.[0].tab_id // empty)' 2>/dev/null || true
}

# ---- pane 相对位置小工具 ----
# $1 是否正好紧贴在 $2 的右边（同一个 tab 里）
pane_right_of() {
  "$herdr_bin" pane layout --pane "$invoke_pane" 2>/dev/null | jq -r --arg a "${1:-}" --arg b "${2:-}" '
    (.result.layout // .layout) as $l
    | ($l.panes // []) as $ps
    | ($ps | map(select(.pane_id == $a)) | .[0]) as $A
    | ($ps | map(select(.pane_id == $b)) | .[0]) as $B
    | if $A == null or $B == null then "no"
      elif $A.rect.x == ($B.rect.x + $B.rect.width) then "yes" else "no" end' 2>/dev/null || true
}

# agent（= $invoke_pane，你按键所在那格）要永远住最右边：
# fresh 要是贴在它右边，就把两者换个位置。
fresh_left_of_agent() {
  local pid="${1:-}"
  [ -n "$pid" ] && [ -n "$invoke_pane" ] || return 0
  [ "$(pane_right_of "$pid" "$invoke_pane")" = "yes" ] || return 0
  "$herdr_bin" pane swap --source-pane "$pid" --target-pane "$invoke_pane" >/dev/null 2>&1 || true
}

# ---- 把 Fresh pane 摆进你按键的那个 tab，并保证它落在 agent 左边 ----
# 只在跨 tab 时才搬；搬的是 pane 本身，里面的 fresh 进程不会被重启。
# 落点优先挑「我左边那个够宽的邻居」(>= 48 列)：fresh 夹在它和我中间，我这一格原样不动。
# 没有这种邻居（左邻居太窄，或我就是最左边）就分我自己，然后跟 agent 换一下 ——
# 分自己会让 fresh 落在 agent 右边，而你要求 agent 永远最右。
place_fresh_pane() {
  local pid="${1:-}" ptab target
  [ -n "$pid" ] && [ -n "$invoke_tab" ] && [ -n "$invoke_pane" ] || return 0
  # herdr 默认把右键留给自己的 pane 菜单，fresh 的「右键=粘贴」就被抢了。这里把这个
  # pane 的右键交还给它（可逆：herdr pane input <pane> --right-click herdr）。
  "$herdr_bin" pane input "$pid" --right-click pane >/dev/null 2>&1 || true
  [ "$pid" = "$invoke_pane" ] && return 0
  ptab=$(pane_tab "$pid")
  if [ "$ptab" = "$invoke_tab" ]; then
    # 已经在同一个 tab：只在它压在 agent 右边时换回来，其它情况别乱动
    fresh_left_of_agent "$pid"
    return 0
  fi
  target=$("$herdr_bin" pane layout --pane "$invoke_pane" 2>/dev/null | jq -r --arg me "$invoke_pane" '
    (.result.layout // .layout) as $l
    | ($l.panes // []) as $ps
    | ($ps | map(select(.pane_id == $me)) | .[0]) as $my
    | if $my == null then empty
      else ($ps
            | map(select(.pane_id != $me and .rect.width >= 48
                         and (.rect.x + .rect.width) == $my.rect.x))
            | sort_by(-.rect.width) | (.[0].pane_id // empty))
      end' 2>/dev/null || true)
  [ -n "$target" ] || target="$invoke_pane"
  "$herdr_bin" pane move "$pid" --tab "$invoke_tab" --split right \
    --target-pane "$target" --ratio 0.5 --no-focus >/dev/null 2>&1 || true
  [ "$target" = "$invoke_pane" ] && fresh_left_of_agent "$pid"
  return 0
}

fresh_sessions() {
  fresh --cmd "$FRESH_GROUP" list 2>/dev/null | sed -n 's/^  \(fresh-[^ ]*\).*/\1/p'
}
has_session() {
  fresh_sessions | grep -qx "$1"
}

# ---- 会话已开：不用开 pane，下面统一摆位置 + 推文件 ----
if has_session "$name"; then
  :
else
  # ---- 没开：借 herdr-fresh 的 entrypoint 开一个 Fresh pane，等它注册出新会话 ----
  # 不传 --cwd：插件 manifest 记过坑，--cwd 会让插件相对路径的启动脚本解析到
  # 项目目录里然后 127 退出；Fresh 的工作目录由 pane 自己的上下文决定。
  before=$(fresh_sessions)
  # 先按 tab 开（0.9.5 的 split 定位不认 pane id），开完由下面的 place_fresh_pane 搬进
  # 当前 tab；--no-focus 保证这个临时 tab 不抢焦点，所以这一段不会闪。
  open_out=$("$herdr_bin" plugin pane open --plugin herdr-fresh --entrypoint fresh \
    --workspace "$ws" --placement tab --no-focus 2>&1) || true
  spawn_tab=$(printf '%s' "$open_out" | jq -r '.result.plugin_pane.pane.tab_id // empty' 2>/dev/null || true)
  name_new=""
  for _ in $(seq 1 50); do
    # 认领这次新出现的 fresh-* 会话（herdr 注入的 context 才是命名权威，别去猜）
    name_new=$(comm -13 <(printf '%s\n' "$before" | sort) <(fresh_sessions | sort) | head -1)
    [ -n "$name_new" ] && break
    sleep 0.2
  done
  if [ -n "$name_new" ]; then
    name="$name_new"
  elif ! has_session "$name"; then
    printf '\033[1;31mFresh 没起来：等了 10s，workspace %s 里没有新的 fresh 会话\033[0m\n' "$ws"
    printf '\n按回车关闭…'
    read -r _ || true
    exit 1
  fi
fi

# ---- 摆位置：Fresh pane 不在你按键的这个 tab 里就搬过来（已在本 tab 则什么都不做）----
place_fresh_pane "$(fresh_pane)"
# 刚才按 tab 开出来的那个临时 tab 里已经没有 Fresh 了，顺手关掉（里面通常只剩
# auto_open 停靠进去的 Sidebar）。搬失败时这段不执行，绝不误伤 Fresh。
fp=$(fresh_pane)
if [ -n "${spawn_tab:-}" ] && [ -n "$fp" ] && [ "$(pane_tab "$fp")" != "$spawn_tab" ]; then
  "$herdr_bin" tab close "$spawn_tab" >/dev/null 2>&1 || true
fi

# ---- 推文件（0.2.12 的会话命令叫 session，0.4+ 才改名 daemon）----
fresh --cmd "$FRESH_GROUP" open-file "$name" "$target" >/dev/null 2>&1 || {
  printf '\033[1;31m推送失败：fresh session open-file %s %s\033[0m\n' "$name" "$target"
  printf '\n按回车关闭…'
  read -r _ || true
  exit 1
}

# ---- 把 Fresh pane 顶到前面（插件同款手法：zoom on → off，焦点留在它身上）----
pid=$(fresh_pane)
if [ -n "$pid" ]; then
  "$herdr_bin" pane zoom "$pid" --on  >/dev/null 2>&1 || true
  "$herdr_bin" pane zoom "$pid" --off >/dev/null 2>&1 || true
fi
