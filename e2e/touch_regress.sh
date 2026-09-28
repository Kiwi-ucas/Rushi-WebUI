#!/usr/bin/env bash
# touch_regress.sh — mobile sticky-bottom regression test runner.
# Thin wrapper over touch_regress.py (the CDP driver). See the Python
# file for the full explanation: the slow-pull release test needs
# requestAnimationFrame, which only the full Chromium build
# (--headless=new) fires — the headless shell used by freeze_regress
# does not.
#
# Usage:
#   e2e/touch_regress.sh [port]
#     port  : rushi-web port (default 8480)
#
# Exit codes: 0 all scenarios as expected | 1 regression |
#             2 inconclusive
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$HERE/touch_regress.py" "${1:-8480}"
