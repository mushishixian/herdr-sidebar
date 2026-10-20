#!/usr/bin/env bash
# 全局「跳转到文件」—— IDE 的 file finder：
#   ctrl+q → 弹 fzf → 打文件名片段 → 回车在 Fresh 里打开。
#
# 候选 = git 跟踪文件 + 未忽略的未跟踪文件（vendor/node_modules 被 .gitignore
# 天然排除）；不是 git 仓库就退回 fd 扫（同样排除那几个目录）。
# 选中后交给 open-in-fresh.sh 推进当前 workspace 的 Fresh pane。
set -uo pipefail

export PATH="$HOME/.local/bin:$HOME/.pi/agent/bin:$PATH"
cd "${HERDR_ACTIVE_PANE_CWD:-$PWD}" 2>/dev/null || true
root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$root" || exit 1
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

list_files() {
  if git rev-parse --git-dir >/dev/null 2>&1; then
    git ls-files --cached --others --exclude-standard
  elif command -v fd >/dev/null 2>&1; then
    fd -t f -E vendor -E node_modules -E storage . 2>/dev/null
  else
    printf '\033[1;31m不在 git 仓库且没装 fd，列不了文件\033[0m\n按回车关闭…'
    read -r _ || true
    exit 1
  fi
}

# 预览：带行号看前 150 行，够认出是哪个文件
preview_cmd='nl -ba "{}" 2>/dev/null | head -n 150'

count=$(list_files | wc -l)
if [ "$count" -eq 0 ]; then
  printf '\033[1;31m%s 下没列出任何文件\033[0m\n按回车关闭…' "$root"
  read -r _ || true
  exit 1
fi

sel=$(list_files | fzf \
  --reverse \
  --prompt='跳转文件 ❯ ' \
  --header="[ $count 个文件 · 打名字片段筛选 · esc 取消 ]" \
  --preview="$preview_cmd" --preview-window=right:55%:wrap) || exit 0

[ -n "$sel" ] || exit 0

exec "$script_dir/open-in-fresh.sh" "$root/$sel" "$root"
