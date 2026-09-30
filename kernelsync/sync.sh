#!/usr/bin/env bash
# sync.sh — sync the rushi kernel to origin/main while preserving the
# WebUI bridge (rushi serve subcommand + [web] config section) and the
# local working config.
#
# What it does (in the kernel repo):
#   1. fetch origin; back up any local changes to kernelsync/backup/
#   2. git reset --hard origin/main
#   3. re-apply kernelsync/serve-web.patch (the rushi serve + [web] bridge)
#   4. restore kernelsync/config.local.toml as the untracked working config
#   5. rebuild kernel + webui (skippable with --no-build)
#
# The patch is based on kernel commit b812b52. If the kernel remote moves
# on and main.rs changes again, step 3's --check will fail: rebase the
# patch onto the new main.rs (see kernelsync/UPSTREAM_PR.md).
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

echo "==> [1/5] pre-sync state"
git fetch origin
echo "  local main : $(git rev-parse --short main) $(git log -1 --format=%s main)"
echo "  origin/main: $(git rev-parse --short origin/main) $(git log -1 --format=%s origin/main)"

if git rev-parse main 2>/dev/null | grep -qx "$(git rev-parse origin/main)"; then
  echo "  already in sync with origin/main (nothing to reset)"
fi

echo "==> [2/5] back up + reset to origin/main"
if [[ -n "$(git status --porcelain)" ]]; then
  mkdir -p "$KS_DIR/backup"
  ts="$(date +%Y%m%d-%H%M%S)"
  git diff > "$KS_DIR/backup/pre-sync-$ts.diff" || true
  cp -f config.toml "$KS_DIR/backup/config-$ts.toml" 2>/dev/null || true
  echo "  working-tree changes backed up to kernelsync/backup/"
fi
git reset --hard origin/main

echo "==> [3/5] re-apply the WebUI bridge patch"
if git apply --check "$KS_DIR/serve-web.patch"; then
  git apply "$KS_DIR/serve-web.patch"
  echo "  serve-web.patch applied"
else
  echo "ERROR: serve-web.patch does not apply to $(git rev-parse --short HEAD)."
  echo "       The kernel changed main.rs again — rebase the patch"
  echo "       (see kernelsync/UPSTREAM_PR.md)." >&2
  exit 1
fi

echo "==> [4/5] restore local working config"
cp "$KS_DIR/config.local.toml" "$KERNEL/config.toml"
echo "  config.toml restored (untracked local working config)"

if [[ $do_build -eq 1 ]]; then
  echo "==> [5/5] rebuild kernel + webui"
  export CARGO_HOME="$CARGO_HOME"
  export PATH="$CARGO_HOME/bin:$PATH"
  cargo build
  cd "$WEBUI_ROOT"
  cargo build
  echo "  done."
else
  echo "==> [5/5] build skipped (--no-build)"
fi

echo
echo "All synced. Verify:"
echo "  ./target/debug/rushi --help        # should list 'serve'"
echo "  $WORKSPACE_ROOT/run-webui.sh       # launch the WebUI"
