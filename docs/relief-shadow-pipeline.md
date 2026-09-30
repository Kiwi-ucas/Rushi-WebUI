# Relief Shadow Pipeline (v0.5.45) — Design & Development Plan

Unified relief-shadow system for `web-leptos/style.css`. Every raised /
recessed surface resolves to a small set of theme-aware semantic tokens, so
relief is tuned in **one place** and reads consistently.

**This is the v0.5.45 state.** The v0.5.42 omni-directional halo experiment
flattened the neumorphic relief (the left sidebar's 浮雕 / 新拟态 look
disappeared), so v0.5.45 **restores the v0.5.40 baseline directional look
through the token pipeline** and **keeps the 1px `--border` divider lines
dropped**. Details + decisions below.

## Status (v0.5.45)
- **Direction restored to directional top-lit** (baseline v0.5.40 values),
  not omni halos. Token VALUES reverted; element→token mapping kept.
- **Token vocabulary collapsed to 4:** `--shadow`, `--shadow-hover`,
  `--groove`, `--shadow-pressed`. `--shadow-sm` was dropped; compact
  controls share `--shadow` with large surfaces again (baseline parity).
- **Relief restored** on every element the omni rework had flattened
  (sidebar sort/new buttons, selected segment, session cards, keyframes,
  structural bands, `#transcript`/`#main::after` top shadow, left strip).
- **1px `--border` divider lines stay DROPPED** (user decision, see below):
  `.sb-header` bottom, `#context-bar` bottom, `#session-actions` border-bottom.
- **Built + deployed:** `trunk build` → `dist/style-eba8f58d08fd7925.css`.
  The running debug `rushi-web` server (:8480, PID 2292) reads `dist/` from
  disk (rust-embed debug) → **no server restart**. Served CSS verified to
  carry the restored markers.
- Remaining: on-screen visual confirmation (light then dark).

## History (why this version)
1. **v0.5.40 baseline (the "hardcoded" look):** relief came from a small set
   of theme tokens with **directional** values (a soft downward fall + a
   bright top-edge rim → each surface reads as a raised block; recessed
   channels read sunken). This is the look the user calls "correct".
2. **v0.5.42 omni rework:** token values became symmetric `(0 0)` halos and a
   `--shadow-sm` split was added. Then, to chase three "defects", the
   sidebar internals + structural bands were flattened to `none`. Net effect:
   the neumorphic relief was lost ("左侧栏浮雕效果全没了，新拟态设计都没了").
3. **v0.5.45 (this):** revert the VALUES to the directional baseline and
   restore the element mappings, but keep the one thing that was actually
   wanted gone — the 1px `--border` divider lines.

## Decisions (user-confirmed for v0.5.45)
- **Dividers = the 1px `--border` lines.** The "extra dividers" the user
  reported were the hard 1px `var(--border)` lines, not the relief. So:
  **relief (shadows/gradients) is restored; the 1px lines stay removed.**
- **Token count:** my call → restore the baseline **4-token** set (drop
  `--shadow-sm`) for maximum fidelity to the hardcoded look. Compact controls
  and large surfaces both use `--shadow` again.
- **Exceptions stay hard-coded:** `.load-earlier` pill shadow, scrollbar
  thumb shadow, food illustrations (`.w-*`), and the structural `#main::before`
  left-strip / `#main::after` + `#transcript` top-shadows (these are the
  baseline "structural band" relief, not generic chrome).

## Token set (directional, v0.5.40 baseline values)
`tint@x = rgba(var(--shadow-tint), x)` · `rim@x = rgba(var(--shadow-rim), x)`
`--shadow-tint`: light `75,70,55` / dark `0,0,0`.
`--shadow-rim`: light `255,255,255` / dark `255,235,200`.

| token | role | light value | dark value |
|---|---|---|---|
| `--shadow` | raised, all surfaces (cards, panels, bars, compact controls) | `0 2px 4px tint@0.16, 0 1px 1px tint@0.10, inset 0 1px 0 rim@0.55` | `0 2px 4px blk@0.55, 0 1px 1px blk@0.40, inset 0 1px 0 rim@0.05` |
| `--shadow-hover` | raised, hover / lifted (shared by large + small) | `0 3px 8px tint@0.22, 0 1px 3px tint@0.12, inset 0 1px 0 rim@0.55` | `0 3px 8px blk@0.65, 0 1px 3px blk@0.45, inset 0 1px 0 rim@0.06` |
| `--groove` | recessed, sunken at rest (inputs, channels, rings, tracks) | `inset 0 1px 2px tint@0.20, inset 0 -1px 0 rim@0.50` | `inset 0 1px 2px blk@0.55, inset 0 -1px 0 rim@0.05` |
| `--shadow-pressed` | recessed, active / pressed | `inset 0 2px 4px tint@0.20` | `inset 0 2px 4px blk@0.60` |

(`--shadow-up` / `--shadow-up-hover` / `--shadow-side` were vestigial in the
baseline too — defined but referenced by no element — so they stay dropped.)

## Element → token mapping (current)
- **`--shadow` (raised):** `#sidebar` (lit-top rim), `.session-item`
  (`:hover`, `.active`, `.done`, `.drop-target`, `sess-breathe` keyframes,
  `prefers-reduced-motion` running state, `.running.running-bg`),
  `#goal-panel`, `#status-bar`, `.event`, `.ev-assistant`,
  `#loop-indicator .loop-ring::after`, `#sidebar-toggle`, `#theme-toggle`,
  `#sidebar-open`, `#sidebar-expand`, `#session-actions button`,
  `.goal-actions button`, `#status-strip .chip`, `.ctx-round:hover`,
  `.ctx-round-more:hover`, `#welcome #w-new`, `.ev-approval-actions button`,
  `#btn-send`, `.ns-btn`, `.ns-dir:hover`, `.qsel-btn`/`.qsel-opt` (rest).
- **`--shadow-hover` (lift / open / hover):** `#input-module`, `#sess-menu`,
  `#ns-dialog`, `#dc-dialog`, `.sb-seg button.on`, the `:hover` / `.open`
  states of the compact elements above, `.qsel-opt:hover`,
  `#welcome #w-new:hover`, `.goal-actions button:hover`, `.ns-btn:hover`.
- **`--groove` (recessed):** `.ctx-round`, `.ctx-round-more`, `#ctx-track`,
  `#welcome .w-chip`, `#msg-input`, `#loop-indicator .loop-ring`, `.ns-input`,
  `.ns-browser`.
- **`--shadow-pressed` (active):** `#theme-toggle:active`,
  `#session-actions button:active`, `#welcome #w-new:active`,
  `#msg-input:focus`, `#btn-send:active`, `.ns-btn:active`.

## Exceptions (deliberately off the pipeline — hard-coded)
- **1px structural divider lines — DROPPED (user decision, v0.5.45):**
  `.sb-header` bottom (`box-shadow: none`), `#context-bar` bottom
  (`box-shadow: none`), `#session-actions` (no `border-bottom`). Everything
  else that uses a 1px `var(--border)` (`.event` card borders, `#goal-panel` /
  `#status-bar` border-top, input/dialog 1px borders) is **unchanged** from
  baseline — they are card/chrome styling, not section dividers.
- **`#loop-indicator .loop-cmd`:** tokenized to `var(--groove)`; baseline had
  a slightly-stronger hard-coded groove (tint@0.30 / rim@0.35 vs token
  0.20/0.50). Kept tokenized for pipeline consistency; visually near-identical.
- **Structural bands:** `#main::before` = 14px left-edge
  `linear-gradient(to right, tint@0.16 → 0)` relief strip (restored);
  `#main::after` = full-width top `inset 0 10px 14px -10px tint@0.30`
  + `border-top-left-radius:14px` (restored — this is the R-corner);
  `#transcript` = same top inset shadow (restored; disabled in flat-mode).
- **Scrollbar:** track = sunken channel (hard-coded); thumb = hard-coded
  raised relief `0 1px 4px tint@0.55, inset 0 1px 0 rim@0.45, inset 0 -1px 0
  tint@0.30` (dark: `0 1px 4px blk@0.60, inset 0 1px 0 rim@0.08`), with dark
  overrides re-added.
- **`.load-earlier` pill:** hard-coded `0 1px 2px tint@0.18, inset 0 1px 0
  rim@0.5` (dark: `0 1px 2px blk@0.50, inset 0 1px 0 rim@0.05`), with dark
  override re-added.
- **Food illustrations:** `#welcome .w-*` keep their own colors/shadows.
- **`#sidebar` (full-height column):** keeps only the lit-top rim
  (`inset 0 1px 0 rim@0.40`; dark `rim@0.05`) — a full-height column stays
  halo-free; the internal relief (buttons, cards) is what carries the
  neumorphic look.

## The three original defects — how they resolve in v0.5.45
- **Defect 1 "R-corner gone":** restored by `#main::after`'s full-width top
  inset shadow + `border-top-left-radius:14px` (baseline behavior). The R-corner
  reads as a soft rounded shade again.
- **Defect 2 "top + left dividers on main face":** the *left* divider is the
  `#main::before` left relief strip (restored, reads as recessed depth, not a
  line) and the *top* is the band shadow; the hard 1px `#context-bar` line is
  gone. Net: depth is back, hard lines are not.
- **Defect 3 "sidebar cards + sort button dividers":** the 1px
  `#session-actions`/`.sb-header` lines stay removed; the sort/new buttons and
  session cards regain their **raised** `--shadow`/`--shadow-hover` relief, so
  they read as discrete neumorphic controls, not flat dividers.

## Build & verify
- `cd web-leptos && CARGO_HOME=<repo>/.cargo-home PATH=<repo>/.cargo-home/bin:$PATH trunk build`
  → regenerates `dist/style-<hash>.css` (Trunk.toml `target = "dist"`).
- The **debug** `rushi-web` server reads `dist/` from disk (rust-embed
  debug) → a browser refresh of :8480 picks it up; **no server restart**
  (in-flight agent loops keep running).
- Verify on screen, light then dark: raised surfaces read as lifted blocks
  (downward fall + bright top rim), recessed inputs read sunken, the
  top-left R-corner is present, the left edge reads recessed, and there are
  **no hard 1px divider lines** under the sidebar header, context bar, or
  sort row.

## Deploy log
- v0.5.45 `trunk build` → `dist/style-eba8f58d08fd7925.css`. Served CSS
  verified: baseline directional `--shadow` (`0 2px 4px …`), both top inset
  shadows (`inset 0 10px 14px -10px`, on `#main::after` + `#transcript`),
  left relief strip, accent-bar card states, hard-coded `.load-earlier` /
  scrollbar-thumb shadows (+dark overrides) all present; **0** references to
  `--shadow-sm` / omni `0 0 6px …` values; **0** `inset 0 -1px 0
  var(--border)` 1px divider lines (the 1px `--border` top/bottom edges of
  `.event` / `.ev-assistant` cards remain — they are baseline card styling,
  not section dividers).
- The running debug `rushi-web` server on :8480 serves the new hash from
  disk — refresh the browser to see it (no restart; in-flight loops keep
  running).
- Note: `web-leptos/src/ui.rs` (parallel workstream, v0.5.44 dispatch cards)
  went through transient compile states (E0525 / E0382) during this deploy;
  it compiled clean for the final build and no changes were made to
  `ui.rs` by the CSS work.
