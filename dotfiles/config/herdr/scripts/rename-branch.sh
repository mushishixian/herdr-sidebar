#!/usr/bin/env bash
# 重命名当前分支（对应 Zed 的 git::RenameBranch）。
#
# 弹窗里直接改：回车确认，**Esc 直接取消**（弹窗立刻关，不用再按回车），退格 /
# ctrl+u / ctrl+d 都能用。
#
# 原来这里用 `read -r` 读一行，问题是行编辑模式下 Esc 只是个字符不是按键 —— 按了
# Esc 取消不掉，反而会被当成新名字的一部分，于是 `git branch -m $'\e'` 报错。要
# 让 Esc 立刻生效只能自己读原始按键，所以下面有个小行编辑器。
set -uo pipefail

# popup 的 cwd 未必是你按快捷键时那个 pane 的目录，herdr 把 pane 的目录放在这个
# 变量里（跟 branch-switch.sh / branch-merge.sh 同一段逻辑）。
cd "${HERDR_ACTIVE_PANE_CWD:-$PWD}" 2>/dev/null || true

# 改名之后要叫 refresh-pane.sh 把 pane 的提示符重画一遍（说明见那个脚本）。
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

# popup 在命令退出时就关掉，输出会一闪而过，所以结尾停住等回车。
hold() {
	printf '\n按回车关闭…'
	read -r _ || true
}

if ! cur=$(git rev-parse --abbrev-ref HEAD 2>/dev/null); then
	echo "不在 git 仓库里：$PWD"
	hold
	exit 1
fi

# 读一行输入：回车 → 返回 0，结果在 REPLY_NAME；Esc / ctrl-c / ctrl-d / EOF → 返回 1。
prompt_name() {
	local prompt="$1" buf="" ch seq
	printf '%s' "$prompt"
	while :; do
		IFS= read -rsn1 ch || {
			printf '\n'
			return 1
		}
		case "$ch" in
		'' | $'\n' | $'\r')
			# 回车。注意 `read -n1` 会把行终止符吃掉、返回空串而不是 '\n'，
			# 所以空串在这里表示「确认」；真正的 EOF 由上面 read 的非零返回码抓。
			printf '\n'
			REPLY_NAME="$buf"
			return 0
			;;
		$'\033')
			# 单独一个 Esc（30ms 内没有后续字节）= 取消。
			# 若紧跟字节，那多半是方向键一类的转义序列（CSI：ESC [ 参数 终结字节），
			# 要精确吃到终结字节（ASCII @-~）为止 —— 不能定长吞几个字节，实测那样
			# 按 ↑ 再打 foo 会把 f 一起吃掉变成 oo。
			if IFS= read -rsn1 -t 0.03 seq; then
				case "$seq" in
				'[' | 'O')
					local i=0
					while [ "$i" -lt 6 ]; do
						IFS= read -rsn1 -t 0.05 seq || break
						case "$seq" in
						[@-~]) break ;;
						esac
						i=$((i + 1))
					done
					continue
					;;
				esac
				# ESC 后面跟着别的字符：当成 Esc+某键的组合，同样算取消
			fi
			printf '\n'
			return 1
			;;
		$'\177' | $'\010') # 退格
			[ -n "$buf" ] || continue
			buf="${buf%?}"
			;;
		$'\025') buf="" ;; # ctrl-u 清空整行
		$'\003') # ctrl-c
			printf '\n'
			return 1
			;;
		[[:cntrl:]]) continue ;; # 其他控制字符直接丢
		*) buf+="$ch" ;;
		esac
		# 整行重画而不是退格覆盖：分支名里可能有中文，逐字节擦会留残影。
		printf '\r\033[K%s%s' "$prompt" "$buf"
	done
}

printf '当前分支: \033[1;33m%s\033[0m\n' "$cur"

if ! prompt_name '新名字 (回车确认 · Esc 取消): '; then
	printf '\033[2m已取消，分支名没变。\033[0m\n'
	exit 0
fi
new="$REPLY_NAME"

if [ -z "$new" ]; then
	echo "名字为空，已取消。"
	exit 0
fi

if [ "$new" = "$cur" ]; then
	echo "和现在一样，没改。"
	exit 0
fi

if git branch -m "$new"; then
	printf '\033[1;32m✔ 已重命名 %s → %s\033[0m\n' "$cur" "$new"
	bash "$script_dir/refresh-pane.sh" "$cur"
fi
hold
