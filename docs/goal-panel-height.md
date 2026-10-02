# Goal Module Height Cap (v0.5.52) — Design & Decision

> **v0.5.53 note:** the cap moved from `#goal-panel` to the shared
> `#plugin-area` when the goal module became the first entry of the sidebar
> plugin area. The 30% value and the "prompt-only scroller" rule are
> unchanged; see `docs/plugin-area.md` for the general design.

The left-sidebar goal module (`#goal-panel`) had **no height bound**: its
prompt (`.goal-text`, `white-space: pre-wrap`) grew with the goal text and
could fill the whole sidebar column, crushing the session list
(`#session-list`, `flex: 1; overflow-y: auto`) to ~0 and pushing the session
cards off-screen.

**Fix (v0.5.52):** cap the module, and let the prompt scroll **inside** that
cap so the session list always keeps the remaining space.

## Decision (user-confirmed)
- **Cap value = 30%** of the sidebar column. (Earlier plan proposed 45%; the
  user chose the tighter 30%.)
- **Sole scroller = the prompt text only.** The status badge, the meta line,
  and the action buttons stay pinned; only `.goal-text` scrolls within the cap.

## What changed (`web-leptos/style.css`, pure CSS — no `ui.rs` / `model.rs` edit)
- `#goal-panel` → `min-height: 0; max-height: 30%;` (caps the module).
- `#goal-body` → `flex: 0 1 auto; min-height: 0; display: flex;
  flex-direction: column;` (flex column so the text can be the sole scroller).
- `#goal-body .goal-text` → `flex: 0 1 auto; min-height: 0; overflow-y: auto;`
  (the prompt is the only thing that scrolls).
- `.goal-header`, `.goal-meta`, `.goal-empty`, `.goal-badge`, `.goal-actions`
  → `flex: 0 0 auto` (pinned; the badge also `align-self: flex-start`).
- `.goal-text` joined the **capsule-in-groove scrollbar group** (the 5 unified
  `::-webkit-scrollbar*` / Firefox `scrollbar-width/color` selector lists) so
  its scrollbar matches every other scroller.

## Why the scroller is `.goal-text` and not `#goal-body`
Capping the panel alone would leave a flat, unscrollable box if the prompt is
long. Making `.goal-text` the sole flex scroller keeps the badge / meta /
actions legible and only the (potentially very long) prompt scrolls —
matching "goal提示词内容单独在这个高度内做滚动条".

## Build & deploy
- `trunk build` → `dist/style-c89ebf2a09c07938.css` (served by the running
  debug `rushi-web` on :8480 — **refresh** to pick it up; no restart).
- Served CSS verified: `max-height: 30%` on `#goal-panel`, `.goal-text`
  scroller + pinned badge/meta/actions present, and `#goal-body .goal-text`
  in all 5 capsule scrollbar lists.
- **Not committed yet** — the working tree carries a parallel workstream's
  uncommitted Rust changes (`lib.rs`, `model.rs`, `transcript.rs`, `ui.rs`,
  `ws.rs`, `win.rs`). This CSS work is isolated to `style.css`; commit only
  when that tree is clean (see inv-0003: don't bundle another session's work).

## Verify on screen
1. Light + dark theme: a **short** goal renders as before (no scrollbar,
   compact).
2. A **very long** goal prompt: the goal panel tops out at ~30% of the
   sidebar; the prompt scrolls within it; the badge / meta / action buttons
   stay visible; the **session cards remain visible** below.
