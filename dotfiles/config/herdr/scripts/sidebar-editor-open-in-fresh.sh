#!/usr/bin/env bash
# herdr-sidebar「Custom editor」的适配层 —— 不改插件源码版。
#
# 背景：sidebar 把「点击文件」实现成「新建一个 `文件名 · editor` tab → 在那个 tab 里
# 跑这条命令」。我们不要那个 tab，也不要它自带的只读预览，所以把它当**路由器**用：
#   1. 文件推进当前 workspace 的 fresh-<ws> 会话（就是 open-in-fresh.sh 那个常驻在你
#      工作 tab 里、紧挨 agent 的 fresh pane）；
#   2. 把 sidebar 为这次点击建出来的空 tab 关掉。
#
# 代价：会闪一下那个空 tab —— sidebar 建完 tab 会立刻把视图切过去，我们只能在命令被
# 拉起来之后才来得及关。（想彻底不闪得改插件源码，那条路你否了。）
#
# fresh 会话还没开时退化为「就在这个 tab 里开 fresh」，保持可用；之后按一次 ctrl+q
# 把 fresh pane 建起来，再点就都走上面那条路由了。
set -uo pipefail

export PATH="$HOME/.local/bin:$PATH"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
# fresh 0.4+ 把 `--cmd session ...` 改名成 `--cmd daemon ...`（0.5.3 里 session 还是别名，
# 但别赌它一直在）。先试 daemon，不行退回 session。
fresh_group() {
  if fresh --cmd daemon list >/dev/null 2>&1; then printf 'daemon'; else printf 'session'; fi
}
FRESH_GROUP="$(fresh_group)"
file="${HERDR_SIDEBAR_EDITOR_FILE:?HERDR_SIDEBAR_EDITOR_FILE 没传进来}"
tab="${HERDR_TAB_ID:-}"
ws="${HERDR_WORKSPACE_ID:-}"
name="fresh-$ws"

fresh_sessions() {
  fresh --cmd "$FRESH_GROUP" list 2>/dev/null | sed -n 's/^  \(fresh-[^ ]*\).*/\1/p'
}

if fresh_sessions | grep -qx "$name"; then
  if ! fresh --cmd "$FRESH_GROUP" open-file "$name" "$file" >/dev/null 2>&1; then
    printf '\033[1;31m推送失败：fresh $FRESH_GROUP open-file %s %s\033[0m\n' "$name" "$file"
    printf '\n按回车关闭…'
    read -r _ || true
    exit 1
  fi
  # 把 fresh pane 顶到前台。它跟 agent 在同一个 tab，所以视图不会离开你的工作区。
  pid=$("$herdr_bin" pane list 2>/dev/null | jq -r --arg n "$name" '
    (.result.panes // []) | map(select(.label == $n)) | (.[0].pane_id // empty)' 2>/dev/null || true)
  if [ -n "$pid" ]; then
    "$herdr_bin" pane zoom "$pid" --on  >/dev/null 2>&1 || true
    "$herdr_bin" pane zoom "$pid" --off >/dev/null 2>&1 || true
  fi
  # 关掉 sidebar 为这次点击建出来的 editor tab（里面只剩它自己和 auto-dock 的 Sidebar）。
  # 放在最后：这条通常是本脚本的最后一步，即使关 tab 把这个 pane 一起收掉也无所谓。
  if [ -n "$tab" ]; then
    "$herdr_bin" tab close "$tab" >/dev/null 2>&1 || true
  fi
  exit 0
fi

# fresh 还没开（比如 herdr 重启过、或者 fresh 被 ctrl+q 退掉了）：就在 sidebar 建的这个
# tab 里把它开起来。会话名和 pane 的 label 都对齐成 fresh-<ws>，这样之后按 ctrl+q 能直接
# 接上它 —— open-in-fresh.sh 会发现这个 pane 不在你的工作 tab 里，把它搬回去，自动收拾干净。
# 注意：`fresh -a <名字> <文件>` 只 attach，不会打开那个文件，所以这里后台等会话注册好再推一次。
if [ -n "${HERDR_PANE_ID:-}" ]; then
  "$herdr_bin" pane rename "$HERDR_PANE_ID" "$name" >/dev/null 2>&1 || true
fi
(
  for _ in $(seq 1 50); do
    fresh_sessions | grep -qx "$name" && break
    sleep 0.2
  done
  fresh --cmd "$FRESH_GROUP" open-file "$name" "$file" >/dev/null 2>&1
) </dev/null >/dev/null 2>&1 &
exec fresh -a "$name"
