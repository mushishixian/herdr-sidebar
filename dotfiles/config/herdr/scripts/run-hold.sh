#!/usr/bin/env bash
# 在 herdr popup 里跑一条命令，并把结果留在屏幕上。
# popup 会在命令退出时立刻关闭，输出一闪而过看不清，所以结尾停住等回车。
#
# 用法: run-hold.sh <标签> <命令> [参数...]
set -uo pipefail

label="${1:-command}"
shift || true

if [ "$#" -eq 0 ]; then
	echo "用法: run-hold.sh <标签> <命令> [参数...]" >&2
	exit 2
fi

printf '\033[1;36m==> %s\033[0m  (%s)\n\n' "$label" "$PWD"
"$@"
code=$?

printf '\n\033[1;36m==> %s 结束，退出码 %d\033[0m\n按回车关闭…' "$label" "$code"
read -r _ || true
exit "$code"
