#!/usr/bin/env bash
# freeze_regress.sh — v0.5.23 UI-freeze regression test runner.
# Thin wrapper over freeze_regress.py (the CDP driver). See the Python
# file for the full explanation: the in-app ?test=freeze flood test
# must be observed in REAL time through CDP, because the headless
# shell's --dump-dom only fires at page load and virtual-time
# budgets mask the responsiveness signal.
#
# Usage:
#   e2e/freeze_regress.sh [port] [mode]
#     port  : rushi-web port (default 8480)
#     mode  : "default" (expect PASS, exit 0)
#              "raw"   (expect FAIL, exit 0)
#              "both"  (run both + re-check, default)
#
# Exit codes: 0 expected verdict | 1 contradicted | 2 inconclusive
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$HERE/freeze_regress.py" "${1:-8480}" "${2:-both}"
