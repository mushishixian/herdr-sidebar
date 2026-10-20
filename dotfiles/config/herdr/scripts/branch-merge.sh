#!/usr/bin/env bash
# 合并分支到当前分支 —— 弹窗里选源分支，确认后合并。
#
# 对应 Zed 的 task "Git: Merge Branch (lazygit)"，但不用进 lazygit：只挑一个分支名
# 然后确认，比在 lazygit 里翻面板快。远程分支也能选（git merge origin/main 是合法
# 操作）。
#
# 用 --no-edit 挡住合并提交时的编辑器：在 popup 里起 vim 是要出事的那种交互。
# 冲突时不自动做任何补救，把 git 的原话和冲突列表留在屏幕上给你看。
set -uo pipefail

# 见 branch-switch.sh 里同一段注释。
cd "${HERDR_ACTIVE_PANE_CWD:-$PWD}" 2>/dev/null || true

# 合并之后要叫 refresh-pane.sh 把 pane 的提示符重画一遍（说明见那个脚本）。
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

if ! git rev-parse --git-dir >/dev/null 2>&1; then
	printf '\033[1;31m不在 git 仓库里：%s\033[0m\n' "$PWD"
	printf '\n按回车关闭…'
	read -r _ || true
	exit 1
fi

cur=$(git symbolic-ref --quiet --short HEAD || true)
if [ -z "$cur" ]; then
	printf '\033[1;31m现在不在任何分支上（detached HEAD），先切个分支再合并。\033[0m\n'
	printf '\n按回车关闭…'
	read -r _ || true
	exit 1
fi

list_rows() {
	git for-each-ref --sort=-committerdate \
		--format='L	%(refname:short)	%(committerdate:relative)' refs/heads
	git for-each-ref --sort=-committerdate \
		--format='R	%(refname:short)	%(committerdate:relative)' refs/remotes |
		while IFS=$'\t' read -r k ref when; do
			[ -n "${ref:-}" ] || continue
			case "$ref" in */HEAD) continue ;; esac
			printf '%s\t%s\t%s\n' "$k" "$ref" "$when"
		done
}

# 自己是当前分支，合并自己没有意义，筛掉。
rows=$(list_rows | grep -Fv "$(printf 'L\t%s\t' "$cur")" || true)
[ -n "$rows" ] || {
	printf '\033[1;33m没有别的分支可以合并进来。\033[0m\n'
	printf '\n按回车关闭…'
	read -r _ || true
	exit 0
}

# --nth=1 而不是 2，理由同 branch-switch.sh：--nth 是对 --with-nth 转换后的行编号。
sel=$(printf '%s\n' "$rows" | fzf \
	--delimiter=$'\t' --with-nth=2,3 --nth=1 \
	--reverse \
	--prompt='合并哪个分支 ❯ ' \
	--header="合并到当前分支: $cur") || exit 0

[ -n "$sel" ] || exit 0

IFS=$'\t' read -r _ ref _ <<<"$sel"

printf '\033[1;36m把 \033[1;33m%s\033[0m\033[1;36m 合并进 \033[1;33m%s\033[0m\033[1;36m? \033[0m[y/N] ' "$ref" "$cur"
read -r a || true
case "${a:-}" in
y | Y | yes | YES) ;;
*)
	echo "已取消。"
	exit 0
	;;
esac

echo
if out=$(git merge --no-edit "$ref" 2>&1); then
	# 合并不改分支名，但 p10k 的状态段（脏 / 领先落后）变了，提示符同样是旧的。
	bash "$script_dir/refresh-pane.sh" "$cur"
	printf '\033[1;32m✔ 已把 %s 合并进 %s\033[0m\n\n%s\n' "$ref" "$cur" "$out"
	printf '\n按回车关闭…'
	read -r _ || true
	exit 0
fi

printf '\033[1;31m✘ 合并没成功\033[0m\n\n%s\n' "$out"
printf '\n按回车关闭…'
read -r _ || true
exit 1
