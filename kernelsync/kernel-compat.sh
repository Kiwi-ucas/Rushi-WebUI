#!/usr/bin/env bash
# kernel-compat.sh — verify the kernel's contract surface still matches what
# rushi-webui assumes. Run after every kernel sync (sync.sh), and before
# starting a session against an updated kernel.
#
# The webui is self-wired: it reads config.toml itself and spawns `rushi run`
# on PATH. Its whole contract with the kernel is:
#   - the append-only events.jsonl vocabulary,
#   - the ext_status marker ids the renderer knows,
#   - the loop CLI it spawns (`rushi run <session> [task]`),
#   - the session-dir layout it reads (cwd, loop.pid, goal-*.json),
#   - the [hooks] pipeline schema the kernel loads from config.toml.
#
# Exit: 0 = compatible, 1 = contract drift detected (details printed).
set -uo pipefail

KS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"      # .../rushi-webui/kernelsync
WEBUI_ROOT="$(cd "$KS_DIR/.." && pwd)"                       # .../rushi-webui
KERNEL="$(cd "$WEBUI_ROOT/../rushi" && pwd)"
BIN="$KERNEL/target/debug/rushi"
EV="$KERNEL/crates/rushi/src/event.rs"
CFG="$KERNEL/config.toml"

fail=0
ok()   { echo "  [ok]  $1"; }
bad()  { echo "  [BAD] $1"; fail=1; }

echo "1) Event vocabulary the webui renderer covers (14 types):"
EVENTS=(user_message assistant_message tool_call tool_result error ext_status
        compaction_started compaction_summary compaction_failed
        context_exhausted approval_request approval rewind user_message_retract)
missing=""
for t in "${EVENTS[@]}"; do
  grep -q "\"$t\"" "$EV" || missing="$missing $t"
done
[[ -z "$missing" ]] && ok "all 14 event types present in event.rs" \
                    || bad "missing in kernel:$missing — extend the webui renderer"

echo "2) Kernel tree carries no local patches (front-ends self-wire):"
if [[ -z "$(git -C "$KERNEL" status --porcelain)" ]]; then
  ok "kernel working tree clean"
else
  bad "kernel tree is dirty — the webui must not depend on a local kernel patch"
fi

echo "3) Loop command the webui spawns (rushi run <session> [task]):"
if [[ ! -x "$BIN" ]]; then
  echo "  [skip] $BIN not built yet — run cargo build in the kernel"
elif "$BIN" run --help 2>&1 | grep -qi "SESSION"; then
  ok "rushi run <session> still accepted"
else
  bad "rushi run CLI changed — check webui loop spawning"
fi
if [[ -x "$BIN" ]] && ! "$BIN" --help 2>&1 | grep -qw "tui\|serve"; then
  ok "kernel exposes no front-end launcher subcommand (as decided upstream)"
fi

echo "4) Rewind mode values the webui writes ('before' | 'on'):"
if grep -q "On" "$EV" && grep -q "Before" "$EV"; then
  ok "kernel RewindMode = {Before, On} (matches webui append_rewind)"
else
  bad "kernel RewindMode changed — check webui append_rewind / REST docs"
fi

echo "5) Working config the webui and the loop share:"
if [[ ! -f "$CFG" ]]; then
  bad "config.toml missing — restore from kernelsync/config.local.toml"
else
  if grep -qE '^\[web\]' "$CFG"; then
    ok "config.toml has [web] (webui reads host/port from it)"
  else
    bad "config.toml missing [web]"
  fi
  # A relative sessions_root resolves against the LOOP's cwd, not the config
  # dir; the webui spawns loops in the session working directory, so the
  # value must be absolute or the loop looks in <cwd>/sessions.
  if grep -qE '^sessions_root\s*=\s*"/' "$CFG"; then
    ok "sessions_root is absolute (loop cwd cannot redirect it)"
  else
    bad "sessions_root must be an absolute path in config.toml"
  fi
  if grep -qE '^\[\[hooks\.on\]\]' "$CFG"; then
    bad "legacy [[hooks.on]] present — the v0.1.5 kernel exits(1) on it"
  elif grep -qE '^\[hooks\.defs\.' "$CFG" && grep -qE '^\[hooks\.pipeline\.' "$CFG"; then
    ok "hooks use the v0.1.5 pipeline schema (defs + per-window steps)"
  else
    bad "no [hooks.defs]/[hooks.pipeline] — check the hook migration"
  fi
fi

echo
if [[ $fail -eq 0 ]]; then
  echo "COMPAT: kernel $(git -C "$KERNEL" rev-parse --short HEAD) is compatible with rushi-webui"
else
  echo "INCOMPATIBLE: see [BAD] items above"
fi
exit $fail
