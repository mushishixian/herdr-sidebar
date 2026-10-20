#!/usr/bin/env bash
# 对应 Zed 的 task "Git Force Reset & Clean"（git checkout -f && git clean -df）。
# Zed 那边没做确认，这里加一道 y/N，免得在终端里误触把工作区扬了。
set -uo pipefail

printf '\033[1;31m即将在 %s 执行:\033[0m\n  git checkout -f\n  git clean -df\n\n' "$PWD"
printf '确认? [y/N] '
read -r a || true

case "${a:-}" in
y | Y | yes | YES) ;;
*)
	echo "已取消。"
	printf '\n按回车关闭…'; read -r _ || true
	exit 0
	;;
esac

echo
if git checkout -f && git clean -df; then
	printf '\n\033[1;32m✔ 完成\033[0m\n'
fi
printf '\n按回车关闭…'; read -r _ || true
