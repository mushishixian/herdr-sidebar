#!/usr/bin/env bash
# adopt.sh — bring a machine's own version of a managed file back into the repo.
#
#   ./adopt.sh ~/.zshrc              show the diff between your file and the repo's
#   ./adopt.sh ~/.zshrc --apply      write your version into the repo, then re-link
#   ./adopt.sh --list                every managed path install.sh knows about
#
# Why this exists: the installer manages files by SYMLINK, so editing one usually
# edits the repo (no syncing needed — commit and pull). But "atomic save" tools
# (sed -i, some editors, a settings UI) replace the symlink with a regular copy;
# from then on the repo and the machine drift apart silently. This finds that
# copy, shows exactly what is different, and on --apply puts your content into
# the repo (git is the backup) and restores the link.
#
# It never touches a managed path except to re-link it after --apply, and it
# refuses a symlink that points somewhere else (an older checkout, say): that is
# a merge, not an adoption.
set -uo pipefail

DOTFILES=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
REPO=$(cd "$DOTFILES/.." && pwd)

usage() { sed -n '2,20p' "$0"; }

target=''
apply=0
list=0
for arg in "$@"; do
  case $arg in
    --list) list=1 ;;
    --apply) apply=1 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) target=$arg ;;
  esac
done

# The checklist is install.sh's plan, so this only ever offers paths the
# installer really manages (same trick verify.sh uses).
plan=$("$DOTFILES/install.sh" --dry-run --no-packages --no-lsp 2>&1)
if [ $? -ne 0 ]; then
  echo "install.sh --dry-run failed; cannot list managed paths" >&2
  exit 1
fi
links=$(printf '%s\n' "$plan" | grep '^PLAN')

# abs PATH — absolute form of the argument, without resolving the final symlink.
abs() {
  local p=$1
  if [ "$p" = "~" ]; then
    p=$HOME
  else
    # leading ~ -> $HOME (no quoted tilde, which never expands)
    p=${p/#\~/$HOME}
  fi
  case $p in
    /*) ;;
    *) p=$PWD/$p ;;
  esac
  printf '%s/%s\n' "$(cd "$(dirname "$p")" 2>/dev/null && pwd)" "$(basename "$p")"
}

if [ "$list" = 1 ]; then
  while IFS=$'\t' read -r _ kind dest src; do
    [ "$kind" = LINK ] || continue
    if [ -L "$dest" ] && [ "$(readlink "$dest")" = "$src" ]; then
      state=linked
    elif [ -e "$dest" ]; then
      state=COPY
    else
      state=missing
    fi
    printf '%-8s %s\n' "$state" "$dest"
  done <<<"$links"
  exit 0
fi

[ -n "$target" ] || { usage; exit 2; }
want=$(abs "$target")

src=''
dest=''
while IFS=$'\t' read -r _ kind d s; do
  [ "$kind" = LINK ] || continue
  if [ "$d" = "$want" ]; then
    dest=$d
    src=$s
    break
  fi
done <<<"$links"

if [ -z "$dest" ]; then
  echo "$target is not a path this installer manages." >&2
  echo "run: $0 --list" >&2
  exit 1
fi

if [ -L "$dest" ]; then
  got=$(readlink "$dest")
  if [ "$got" = "$src" ]; then
    printf '%s is already the repo symlink.\n' "$dest"
    printf 'Edit the repo file directly, then:  git -C %s commit -am "…"\n' "$REPO"
    exit 0
  fi
  echo "$dest is a symlink to $got, not to $src." >&2
  echo "That is a different checkout or a hand-made link: merge the two by hand." >&2
  exit 1
fi

if [ ! -e "$dest" ]; then
  echo "$dest does not exist (the installer would just link it)." >&2
  echo "run: dotfiles/install.sh" >&2
  exit 1
fi

echo "=== your copy: $dest"
echo "=== repo file: $src"
echo
diff -u "$src" "$dest" || true
echo

if [ "$apply" != 1 ]; then
  printf 'Nothing changed. To take YOUR version into the repo and restore the link:\n'
  printf '    %s %s --apply\n' "$0" "$target"
  exit 0
fi

cp -p "$dest" "$src"
ln -sfn "$src" "$dest"
printf 'wrote %s from %s and re-linked it\n' "$src" "$dest"
printf '\nnext:\n'
printf '    git -C %s status --short\n' "$REPO"
printf '    git -C %s commit -am "config: adopt %s"\n' "$REPO" "$(basename "$dest")"
printf '    git -C %s push\n' "$REPO"
printf 'other machines pick it up with: git pull\n'
