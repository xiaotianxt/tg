#!/usr/bin/env bash
# Run a command against this working tree on another Mac, so builds and test
# loops do not load this machine.
#
#   scripts/remote.sh make check
#   scripts/remote.sh go test ./internal/...
#
# TG_REMOTE names the ssh host (default: mini); TG_REMOTE_DIR the checkout
# there. The tree is mirrored without build outputs or local chat data.
set -euo pipefail

host="${TG_REMOTE:-mini}"
dir="${TG_REMOTE_DIR:-dev/tg-remote}"
repo="$(cd "$(dirname "$0")/.." && pwd)"

[ $# -gt 0 ] || { sed -n '2,10p' "$0"; exit 2; }

rsync -a --delete \
  --exclude .git/ --exclude target/ --exclude .tg/ --exclude exported/ \
  --exclude '*.db' --exclude '*.db-wal' --exclude '*.db-shm' --exclude 'diff[0-9]' \
  "$repo/" "$host:$dir/"

# The remote login shell may not be POSIX; run the command under bash.
printf -v cmd '%q ' "$@"
ssh "$host" bash -lc "$(printf '%q' "cd ~/$dir && $cmd")"
