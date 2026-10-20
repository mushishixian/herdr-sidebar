#!/usr/bin/env bash
# ctrl+shift+a：git add -A + 用 opencommit（oco）生成的消息直接提交。
#
# 为什么不直接绑 `oco`：oco 没有 --all 这类一键全暂存的 flag，暂存区为空时它会
# 弹交互问「要不要 stage all」，在 popup 里多一步没意义。所以这里自己 add -A，
# 再用 `oco -y` 跳过确认——你的 ~/.opencommit 里 OCO_GITPUSH=false，提交完就结束，
# 不会追问 push。
set -uo pipefail

# popup 的 cwd 就是快捷键按下时那个活动 pane 的目录
cd "${HERDR_ACTIVE_PANE_CWD:-$PWD}" 2>/dev/null || exit 1

# 解析 oco 和 node：popup 的 PATH 未必带 fnm 的 multishell 路径，找不到就按
# fnm 安装目录兜底（取版本号最大的那个）。
oco="$(command -v oco 2>/dev/null || true)"
[ -n "$oco" ] || oco="$(ls -1 "$HOME"/.local/share/fnm/node-versions/*/installation/bin/oco 2>/dev/null | sort -V | tail -1)"
if [ -z "$oco" ]; then
	echo "✘ 找不到 oco（opencommit）。装一下：npm i -g opencommit"
	exit 1
fi
cli="$(readlink -f "$oco")"

node="$(command -v node 2>/dev/null || true)"
[ -n "$node" ] || node="$(ls -1 "$HOME"/.local/share/fnm/node-versions/*/installation/bin/node 2>/dev/null | sort -V | tail -1)"
if [ -z "$node" ]; then
	echo "✘ 找不到 node。"
	exit 1
fi

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
	echo "✘ 当前目录不是 git 仓库：$PWD"
	exit 1
}

echo "==> git add -A"
git add -A || exit 1

if git diff --cached --quiet; then
	echo "✓ 没有可提交的改动"
	exit 0
fi

echo "==> 正在用 opencommit 生成提交消息并提交…"
"$node" "$cli" -y
rc=$?

# 提交会改变脏标记（+2 !3 那串），当前提示符上的旧标记过时了。
# 让提示符重画一遍；agent pane 会被 refresh-pane.sh 自己的守卫拦下，不受影响。
if [ "$rc" -eq 0 ]; then
	branch="$(git branch --show-current 2>/dev/null)"
	[ -n "$branch" ] && "$(dirname "$0")/refresh-pane.sh" "$branch"
fi

exit "$rc"
