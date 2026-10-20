#!/usr/bin/env bash
# 像 IDE 一样打开一个新目录（ctrl+alt+n）：
#
#   [ Sidebar 目录树 | 文件预览 | 命令行/agent ]
#
# 做法：fzf 选目录 → herdr workspace create --cwd <目录>。
# herdr-sidebar 的 workspace.created 钩子会自动把目录树靠左停靠（auto_open），
# 根 pane 就是右侧的命令行；之后在树上点/回车一个文件，预览 pane 会 split 到
# sidebar 右边（state.json 的 preview_placement = "pane"），正好落在中间。
set -uo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
cd "${HERDR_ACTIVE_PANE_CWD:-$PWD}" 2>/dev/null || true

# 候选目录：常用项目根下面两层，排除隐藏目录和构建产物
dir=$(find /data/project "$HOME/src" "$HOME" -maxdepth 2 -type d \
	\( -path '*/.*' -o -path '*/node_modules*' -o -path '*/target*' -o -path '*/go/*' \) -prune \
	-o -type d -print 2>/dev/null |
	sort -u |
	fzf --prompt='打开目录 ▾ ' --height=100% --header='选择要打开的项目目录') || exit 0

[ -n "$dir" ] && [ -d "$dir" ] || exit 0

"$herdr_bin" workspace create --cwd "$dir" --label "$(basename "$dir")" --focus
