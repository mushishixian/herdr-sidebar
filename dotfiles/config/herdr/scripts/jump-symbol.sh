#!/usr/bin/env bash
# 全局「跳转到函数/类」—— IDE 的 go-to-symbol：
#   ctrl+w → 弹 fzf → 打字筛 class/interface/trait/函数/方法 → 回车跳进 Fresh 的那一行。
#
# 候选是 rg 实时扫出来的（不建索引、永远和磁盘同步）：实测本项目 ~2600 个 PHP 定义
# 只要 27ms。awk 顺着 --sort path 的输出跟踪「当前类」，把方法标成 Class::method；
# vendor/storage/node_modules 不扫。
#
# 想加别的语言：在 rg 的 --type 和模式里加一组就行（js/ts 的定义模式不同）。
set -uo pipefail

export PATH="$HOME/.local/bin:$HOME/.pi/agent/bin:$PATH"
cd "${HERDR_ACTIVE_PANE_CWD:-$PWD}" 2>/dev/null || true
root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$root" || exit 1
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

if ! command -v rg >/dev/null 2>&1; then
  printf '\033[1;31m没找到 rg（ripgrep）\033[0m\n按回车关闭…'
  read -r _ || true
  exit 1
fi

# 每行：路径:行号 <TAB> 标签(Class::method / function) <TAB> 原文那一行
list_symbols() {
  rg --vimgrep -n --sort path --type php \
     -g '!vendor/**' -g '!node_modules/**' -g '!storage/**' \
     '^\s*(abstract[ \t]+|final[ \t]+|readonly[ \t]+)*(class|interface|trait|enum)[ \t]+[A-Za-z_]\w*|\bfunction[ \t]+&?[A-Za-z_]\w*[ \t]*\(' . |
  awk -F: '
    {
      path = $1; line = $2;
      src = $4; for (i = 5; i <= NF; i++) src = src ":" $i;
      sub(/^[ \t]+/, "", src);
      if (src ~ /^use[ \t]/) next;                       # use function 导入不算定义
      if (src ~ /^(abstract[ \t]+|final[ \t]+|readonly[ \t]+)*(class|interface|trait|enum)[ \t]+[A-Za-z_]/) {
        match(src, /(class|interface|trait|enum)[ \t]+[A-Za-z_][A-Za-z0-9_]*/);
        head = substr(src, RSTART, RLENGTH);
        split(head, a, /[ \t]+/);
        cur[path] = a[2];                                 # 之后本文件里的 function 都算它的方法
        label = a[2];
      } else if (match(src, /function[ \t]+&?[A-Za-z_][A-Za-z0-9_]*/)) {
        fn = substr(src, RSTART, RLENGTH);
        sub(/^function[ \t]+&?/, "", fn);
        label = (path in cur) ? cur[path] "::" fn : fn;
      } else next;
      printf "%s:%s\t%s\t%s\n", path, line, label, src;
    }'
}

# 预览：定位行高亮，上下各带 10 行
preview_cmd='loc=$(echo -E {} | cut -f1); p=${loc%:*}; l=${loc##*:}
awk -v l="$l" '\''NR==l { printf "\033[1;33m%6d  %s\033[0m\n", NR, $0; next }
NR >= l-10 && NR <= l+10 { printf "%6d  %s\n", NR, $0 }'\'' "$p" 2>/dev/null'

count=$(list_symbols | wc -l)
if [ "$count" -eq 0 ]; then
  printf '\033[1;31m%s 下没扫到 PHP 定义（class/function）\033[0m\n按回车关闭…' "$root"
  read -r _ || true
  exit 1
fi

# --nth=1,2：搜「路径:行」和「标签」两列；--with-nth=2,3：只显示标签和原文
sel=$(list_symbols | fzf \
  --delimiter=$'\t' --with-nth=2,3 --nth=1,2 \
  --reverse \
  --prompt='跳转函数/类 ❯ ' \
  --header="[ $count 个定义 · 打名字或路径片段筛选 · esc 取消 ]" \
  --preview="$preview_cmd" --preview-window=right:55%:wrap) || exit 0

[ -n "$sel" ] || exit 0
loc=$(printf '%s' "$sel" | cut -f1)
abs="$root/${loc%:*}"
line=${loc##*:}

exec "$script_dir/open-in-fresh.sh" "$abs:$line" "$root"
