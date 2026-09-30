#!/usr/bin/env bash
# kernel-compat.sh — verify the kernel's contract surface still matches what
# rushi-webui assumes. Run after every kernel sync (sync.sh), and before
# starting a session against an updated kernel.
#
# Exit: 0 = compatible, 1 = contract drift detected (details printed).
set -uo pipefail

KS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"      # .../rushi-webui/kernelsync
WEBUI_ROOT="$(cd "$KS_DIR/.." && pwd)"                       # .../rushi-webui
KERNEL="$(cd "$WEBUI_ROOT/../rushi" && pwd)"
BIN="$KERNEL/target/debug/rushi"
EV="$KERNEL/crates/rushi/src/event.rs"

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

echo "2) WebUI bridge subcommand:"
if [[ ! -x "$BIN" ]]; then
  echo "  [skip] $BIN not built yet — run cargo build in the kernel"
elif "$BIN" --help 2>&1 | grep -qw serve; then
  ok "rushi binary exposes 'serve' (bridge patch in place)"
else
  bad "rushi serve missing — apply kernelsync/serve-web.patch and rebuild"
fi

echo "3) Loop command the webui spawns (rushi run <session> [task]):"
if [[ -x "$BIN" ]]; then
  if "$BIN" run --help 2>&1 | grep -qi "SESSION"; then
    ok "rushi run <session> still accepted"
  else
    bad "rushi run CLI changed — check webui loop spawning"
  fi
fi

echo "4) Rewind mode values the webui writes ('before' | 'on'):"
if grep -q "On" "$EV" && grep -q "Before" "$EV"; then
  ok "kernel RewindMode = {Before, On} (matches webui append_rewind)"
else
  bad "kernel RewindMode changed — check webui append_rewind / REST docs"
fi

echo "5) Working config still has the [web] section:"
if grep -q '^\[web\]' "$KERNEL/config.toml" 2>/dev/null; then
  ok "config.toml has [web] (rushi serve picks up the webui binary)"
else
  bad "config.toml missing [web] — restore from kernelsync/config.local.toml"
fi

echo
if [[ $fail -eq 0 ]]; then
  echo "COMPAT: kernel $(git -C "$KERNEL" rev-parse --short HEAD) is compatible with rushi-webui"
else
  echo "INCOMPATIBLE: see [BAD] items above"
fi
exit $fail
