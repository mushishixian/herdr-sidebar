#!/usr/bin/env bash
# 切换分支 —— IDE 风格的小弹窗：顶上过滤框，下面实时筛出的分支列表。
#
# 本地分支按最近提交排序置顶；远程分支（origin/*）排在其后，选中即建同名本地
# 跟踪分支。跟 herdr-sidebar 的分支选择器比，这里过滤是模糊匹配，而且不占一个
# 常驻 pane —— 按一下出现，选完就消失。
#
# 成功就直接退出（popup 随之关闭，当前分支看 herdr 侧栏/顶栏就行）；失败会把
# git 的原话留在屏幕上等你回车，免得错误一闪而过看不见。
set -uo pipefail

# popup 的 cwd 未必是你按快捷键时那个 pane 的目录，herdr 把 pane 的目录放在这个
# 变量里。取不到就退回当前目录，总之不比不 cd 差。
cd "${HERDR_ACTIVE_PANE_CWD:-$PWD}" 2>/dev/null || true

if ! git rev-parse --git-dir >/dev/null 2>&1; then
	printf '\033[1;31m不在 git 仓库里：%s\033[0m\n' "$PWD"
	printf '\n按回车关闭…'
	read -r _ || true
	exit 1
fi

# 切完之后要叫 refresh-pane.sh 把 pane 的提示符重画一遍（p10k 的分支段只有下一次
# 提示符才会重算，见那个脚本里的说明）。用脚本自己的目录定位，别写死绝对路径。
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

cur=$(git symbolic-ref --quiet --short HEAD || true)

# 每行三个字段：L/R = 本地/远程，分支名，相对时间。fzf 只显示后两列（--with-nth），
# 但选中时整行原样回来，于是不用猜选中的是本地还是远程。
list_rows() {
	git for-each-ref --sort=-committerdate \
		--format='L	%(refname:short)	%(committerdate:relative)' refs/heads
	git for-each-ref --sort=-committerdate \
		--format='R	%(refname:short)	%(committerdate:relative)' refs/remotes |
		while IFS=$'\t' read -r k ref when; do
			[ -n "${ref:-}" ] || continue
			# origin/HEAD 是个符号引用，不是能切的分支。
			case "$ref" in */HEAD) continue ;; esac
			# 本地已经有同名分支就别再列一遍，切本地那个更省事。
			git show-ref --verify --quiet "refs/heads/${ref#*/}" && continue
			printf '%s\t%s\t%s\n' "$k" "$ref" "$when"
		done
}

# --nth 必须写 1 而不是 2：fzf 手册里写明，--with-nth 和 --nth 同时用时，字段号
# 是对「转换后」的行算的（因为 fzf 不让搜被藏起来的字段）。这里显示的是第 2、3 列，
# 所以转换后第 1 列 = 分支名。写成 2 的话搜的是那列相对时间，打字筛分支会筛不出来。
sel=$(list_rows | fzf \
	--delimiter=$'\t' --with-nth=2,3 --nth=1 \
	--reverse \
	--prompt='切换分支 ❯ ' \
	--header="当前: ${cur:-detached HEAD}") || exit 0

[ -n "$sel" ] || exit 0

IFS=$'\t' read -r kind ref _ <<<"$sel"
name=${ref#*/}

case "$kind" in
L)
	if out=$(git switch "$ref" 2>&1); then
		bash "$script_dir/refresh-pane.sh" "$cur"
		exit 0
	fi
	;;
R)
	if git show-ref --verify --quiet "refs/heads/$name"; then
		if out=$(git switch "$name" 2>&1); then
			bash "$script_dir/refresh-pane.sh" "$cur"
			exit 0
		fi
	elif out=$(git switch --create "$name" --track "$ref" 2>&1); then
		bash "$script_dir/refresh-pane.sh" "$cur"
		exit 0
	fi
	;;
*)
	out="认不出这行选择：$sel"
	;;
esac

printf '\033[1;31m✘ 切换失败\033[0m\n\n%s\n' "$out"
printf '\n按回车关闭…'
read -r _ || true
exit 1
