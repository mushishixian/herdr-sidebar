#!/usr/bin/env bash
# 分支操作之后，让「你按快捷键时那个 pane」的提示符重新画一遍。
#
# 为什么要这么绕：p10k 的分支段是 precmd 里算出来的，提示符一旦画好，就只有下一次
# 提示符（也就是「回车」）会重算。实测 Ctrl-L 没用 —— 它只是把缓存的提示符重画一遍，
# 连时间戳都还是老的，分支名当然也是旧的。
#
# 但回车有风险：输入行里要是留着打了一半的命令，回车会把它提交执行。所以这里先判断
# 「输入行是空的」，判不出来就什么都不做 —— 宁可让提示符旧着，也不替你把命令跑掉。
#
# 用法: refresh-pane.sh <提示符里那个旧分支名>
set -uo pipefail

old="${1:-}"
pane="${HERDR_ACTIVE_PANE_ID:-}"
herdr="${HERDR_BIN_PATH:-herdr}"

[ -n "$pane" ] && [ -n "$old" ] || exit 0

# pane 里在跑 agent（pi / claude / …）就绝不碰它的键盘：那键位是 agent 的，不是
# shell 的，回车可能变成提交一条消息或者按掉一个弹窗。
agent=$("$herdr" pane get "$pane" 2>/dev/null | jq -r '.result.pane.agent // empty') || exit 0
[ -z "$agent" ] || exit 0

# 末行。正在跑命令、或者前台是全屏程序时，末行不会含分支名，下面会直接放过。
line=$("$herdr" pane read "$pane" --source visible --format text 2>/dev/null |
	grep -v '^[[:space:]]*$' | tail -1) || exit 0
[ -n "$line" ] || exit 0

# 分支名得真的出现在末行，否则这不是提示符行。
case "$line" in
*"$old"*) ;;
*) exit 0 ;;
esac

# 取分支名「第一次」出现之后的内容。故意用第一次而不是最后一次：万一你正打了一半的
# 命令里也含这个分支名（比如 `git log branch-a`），第一次出现仍然是提示符里那个分支段，
# 不会被后面那次骗过去。分支段在行首附近，这是这个判断的安全底座。
after=${line#*"$old"}

# 去掉行尾那段 p10k 右对齐的尾巴（状态 + 时钟）：ok / ✘ / 1.2s 再加上 04:23:27 下午。
# 尾巴形状对不上就删不掉，剩下的照样是非空白 → 判成「有内容」→ 不动。
# 也就是说：换个主题、改个提示符，最坏结果是「不刷新」，不会是「误回车」。
stripped=$(printf '%s' "$after" | sed -E \
	's/[[:space:]]*(ok|✘|✔|[0-9]+(\.[0-9]+)?s)?[[:space:]]*[0-9]{1,2}:[0-9]{2}(:[0-9]{2})?[[:space:]]*(上午|下午|AM|PM)?[[:space:]]*$//')

# 再去掉 p10k 跟在分支名后面的那串状态标记：*8（stash）、+2（staged）、!3（unstaged）、
# ?1（untracked）、<2 >1（落后/领先）、WIP、MERGING 这类动作词、:origin/main 这类远端后缀。
# 它们是提示符的一部分，不是你打的字 —— 不去掉的话，仓库一脏就永远判成「有输入」，
# 刷新再也不会触发（liexin 那次没刷新就是这个原因：提示符里挂着个 *8）。
# 形状以你 ~/.p10k.zsh 里 my_git_formatter() 的输出为准。
rest=$(printf '%s' "$stripped" | sed -E '
  :again
  s/^[[:space:]]+//
  s/^WIP([[:space:]]|$)/\1/
  s/^<-[0-9]+//
  s/^->[0-9]+//
  s/^[<>][0-9]+//
  s/^\*[0-9]+//
  s/^~[0-9]+//
  s/^\+[0-9]+//
  s/^![0-9]+//
  s/^\?[0-9]+//
  s/^-[[:space:]]*$//
  s/^[A-Z][A-Z-]*([[:space:]]|$)/\1/
  s/^:[^[:space:]]+//
  t again
  s/^[[:space:]]+//
')

# 标记剥完还剩东西 → 那就是你打的字，不回车。
# 唯一的理论残留风险：你刚好只打了和标记一模一样的字（比如整行就一个 `!3`），
# 那会被当成空行提交 —— 概率极低，真撞上也只是执行了这一行，这里先认了。
case "$rest" in
'' ) ;;
*) exit 0 ;;
esac

"$herdr" pane send-keys "$pane" enter >/dev/null 2>&1 || true
exit 0
