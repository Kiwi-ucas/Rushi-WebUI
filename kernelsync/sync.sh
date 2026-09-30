#!/usr/bin/env bash
# sync.sh — move the rushi kernel to origin/main and restore the local
# working config.
#
# The webui is self-wired (kernel docs/itches.md, 2026-09-20: the kernel
# hosts no front-end launcher subcommands): it reads config.toml itself
# and spawns `rushi run` found on PATH. So there is NO kernel patch to
# carry any more — the kernel tree stays pristine at origin/main, and
# every sync is a plain fast-forward plus the working config.
#
# What it does (in the kernel repo):
#   1. fetch origin; back up any local changes to kernelsync/backup/
#   2. git reset --hard origin/main
#   3. restore kernelsync/config.local.toml as the untracked working config
#   4. rebuild kernel + webui
#
# Usage:
#   ./sync.sh              # full sync incl. rebuild
#   ./sync.sh --no-build   # git work only
set -euo pipefail

KS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"      # .../rushi-webui/kernelsync
WEBUI_ROOT="$(cd "$KS_DIR/.." && pwd)"                       # .../rushi-webui
WORKSPACE_ROOT="$(cd "$WEBUI_ROOT/.." && pwd)"               # .../deepseek_harness/rushi
KERNEL="$WORKSPACE_ROOT/rushi"
CARGO_HOME="$WORKSPACE_ROOT/.cargo-home"

do_build=1
[[ "${1:-}" == "--no-build" ]] && do_build=0

cd "$KERNEL"

echo "==> [1/4] pre-sync state"
git fetch origin
echo "  local main : $(git rev-parse --short main) $(git log -1 --format=%s main)"
echo "  origin/main: $(git rev-parse --short origin/main) $(git log -1 --format=%s origin/main)"

echo "==> [2/4] back up + reset to origin/main"
if [[ -n "$(git status --porcelain)" ]]; then
  mkdir -p "$KS_DIR/backup"
  ts="$(date +%Y%m%d-%H%M%S)"
  git diff > "$KS_DIR/backup/pre-sync-$ts.diff" || true
  cp -f config.toml "$KS_DIR/backup/config-$ts.toml" 2>/dev/null || true
  echo "  working-tree changes backed up to kernelsync/backup/"
fi
git reset --hard origin/main

echo "==> [3/4] restore local working config"
cp "$KS_DIR/config.local.toml" "$KERNEL/config.toml"
echo "  config.toml restored (untracked local working config)"

if [[ $do_build -eq 1 ]]; then
  echo "==> [4/4] rebuild kernel + webui"
  export CARGO_HOME="$CARGO_HOME"
  export PATH="$CARGO_HOME/bin:$PATH"
  cargo build
  cd "$WEBUI_ROOT"
  cargo build
  echo "  done."
else
  echo "==> [4/4] build skipped (--no-build)"
fi

echo
echo "All synced. Verify:"
echo "  $KS_DIR/kernel-compat.sh          # contract check"
echo "  $WORKSPACE_ROOT/run-webui.sh      # launch the WebUI"
