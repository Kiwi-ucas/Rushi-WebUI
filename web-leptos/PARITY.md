# Phase 4/5 parity checklist — Leptos WASM vs legacy `web/dist/index.html`

Status: all items ported and compiled (0 errors, 0 warnings); trunk → dist/
served by `rushi-web` (rust-embed reads disk in debug — refresh the browser
after a build).

## Pile engine (legacy `applyView` / `syncCardUnfold` / `syncCardShrink` / `setPileOpen` / `syncInputGutter`)
- [x] Constants: PILE_TOP=16, PILE_BAND=12, COMPACT_ROW_H=64 (52 row + 12 margin).
- [x] All cards stay in the DOM; the engine adds `.hid` to cards after the view summary.
- [x] ext_status events render an empty view (no DOM node) → card index = k-th
      non-ext_status event (legacy `addEvent` returns early for ext_status).
- [x] Per-round summary cards tagged `data-isSum` (exempt from folding; height still
      positions the cards below). Summary = last *rendered* card of the round
      (ext_status skipped), re-derived from the stream via `compute_rounds`.
- [x] Out-of-range round view falls back to live (legacy `applyView` clamp
      `view > closed → 'live'`; guards stale `view_round` after a session switch).
- [x] `data-natH` natural-height measurement at first sight; re-measured on unfold
      (details toggles can grow a card).
- [x] Scroll-direction-gated docking/dealing (delta from last observed scrollTop),
      PILE_BAND hysteresis, sTop≤1 top drain, click-expanded pile deals everything.
- [x] Pile = a separate fixed `#pile-stack` header module above the transcript
      (NOT a floating card glued over the messages). It shows the REAL folded count
      (a badge + one visible deck layer per folded card, capped at 10) and the top
      folded card's brief; click toggles expand/collapse. The stack, the message
      list, and the input are three distinct relief modules that never overlap.
      The engine reports `pile_count`/`pile_top_brief`/`pile_open` via signals;
      the folded cards stay in the transcript as compact rows (the scroll range).
- [x] Cut-line shrink (legacy `syncCardShrink`, faithful port): the bottom-most
      card straddling the transcript's bottom edge is rendered shorter so its
      frame (border-radius + box-shadow relief) is drawn intact at the cut line,
      and `#scroll-spacer` absorbs the difference (scroll position never moves).
      The cut line is the content-box bottom (border-box bottom − padding-bottom),
      so the drop-shadow relief stays visible in the reserved bottom padding.
- [x] rAF-coalesced stepping (one step per frame); a DOM-not-ready frame reschedules.
- [x] Input gutter: `#input-module` right margin = 16 + (offset_width − client_width);
      re-synced on window resize and sidebar `transitionend`.
- [x] (user-requested deviation, 2025-07) the legacy cut-line shrink is
      DISABLED: the bottom card is never height-clipped — every card keeps
      its natural height so its bottom edge (radius + shadow relief) stays
      intact; `sync_shrink` only releases stale inline styles, spacer = 0.

## Document-level behavior
- [x] `/` focuses `#msg-input` (skipped when the input already has focus).
- [x] Escape closes the session "…" menu; document click outside `#sess-menu`
      closes it (menu state lives in AppState so the handlers can reach it).
- [x] visualViewport keyboard lift: body height pinning on coarse pointers
      (`kb-anim` transition class), near-bottom → keep newest messages in view
      after the lift settles, document-top pin, 500 ms interval poll.

## Context bar / rounds
- [x] ctx fill thresholds: >90% danger, >70% warn, else accent; `~K / 256K` label.
- [x] Round chips: empty 26×10 buttons, info in `title` only
      (`“user text” (brief 40) \n round N · ~K`), `.ctx-spot` hidden placeholders,
      `on` state = viewed round. `ctxK` recorded at each round close (history
      replay + live user_message + optimistic do_send), reset on session switch/delete.
- [x] Chip click → `view_round = Some(i)`; the engine folds/pins the summary and
      parks it at the bottom (legacy smooth scroll → instant, same resting spot).

## Events / sessions
- [x] New Session opens a modal (not a bare prompt): session name + a working
      directory picker (server-driven `/api/browse` directory list, prefilled with
      `/api/default-cwd`). `POST /api/sessions {name, cwd}` records the choice in
      `sessions/<id>/.cwd`; the loop spawn (`process.rs`) uses it as the child cwd,
      falling back to the default (sessions root's parent) when unset.
- [x] WS history replay sets events + ctx_used + rounds_ctxk; live "event" lines
      append (ctx bar updates on assistant usage; user_message records ctxK and
      closes the previous round — mirrors legacy curRound bookkeeping).
- [x] WS "error" lines render as error cards with a ts (legacy
      `new Date().toISOString()`).
- [x] Optimistic user_message push on send (WS open → sendWS, else POST).
      NOTE: legacy double-renders user messages when the WS echoes them back
      (no dedupe in legacy); the port preserves that exact behavior.
- [x] Approval cards resolve from pending-approval events (Memo per card).
- [x] Status strip chips rebuilt from ext_status events on every stream change.

## Goal panel
- [x] create / pause / resume / clear actions; badge status precedence
      blocked > completed > paused > active; meta "iter N · K tok · id" + block reason.
- Minor: legacy skips the goal reload on failed non-400 actions; the port always
      refetches (harmless — state unchanged on failure).

## Layout / mount
- [x] Legacy DOM: `body (flex, 100dvh) > #app (width:100%; height:100%)`.
      Leptos mounts into `#mount-root`, which is `display: contents` so `#app`
      is the flex child exactly like the legacy structure (a plain wrapper div
      would break `#app`'s 100% height → squashed initial screen).
- [x] Boot screen: a `#boot` overlay (outside the mount root) shows "loading…"
      until `TrunkApplicationStarted`, then is removed; an 8 s watchdog turns it
      into an error + reload button instead of a silent infinite "loading…".

## Known acceptable divergences
- setView smooth scroll → instant `set_scroll_top` (same resting position, no
  ScrollBehavior feature needed).
- Cut-line shrink RE-ENABLED (faithful port): the earlier "always full height"
  deviation sliced the bottom card's relief at the container edge; the shrink
  restores the intact frame, and the cut line is offset by the bottom padding so
  the drop-shadow relief survives. See the pile-engine section above.
- Session select parks at the last message: `pile::on_history_loaded()`
  (called from the WS history branch) sets a `park_bottom` flag consumed by
  the next engine step, with a 150 ms re-park after the DOM settles.
- Input font is 14 px (same as `.ev-content` message text) instead of the
  legacy 16 px (iOS auto-zoom threshold — superseded by user request).
- `create_effect` → `Effect::new` (0.7.8 deprecation); `on:mount` does not exist in
  this Leptos version, so engine init runs from the Transcript `Effect` (first tick
  lands after `mount_to`, DOM is ready; init is idempotent and retries on the next
  signal change if the node is still missing). The WS history/event branches call
  `pile::on_history_loaded` / `pile::on_change` as a second, independent trigger.
- Self-heal fold: until the first successful fold (`fold_applied`), every engine
  step forces `reset_fold`, so a DOM-not-ready first frame cannot leave the cards
  permanently unfolded.
- Panics are captured into `LAST_PANIC` (via a Rust panic hook in lib.rs) and
  reported by `__rushiPile()`; a one-shot `[rushi]` console trace marks the first
  successful step.
- ROOT CAUSE of the dead engine (fixed): `init` held the `PILE` cell's
  `borrow_mut()` across its whole body and called `on_vv_event()` inside, which
  re-borrowed the cell → `RefCell already mutably borrowed`. A wasm trap does not
  run `Drop`, so the borrow flag stayed set and **poisoned `PILE` permanently** —
  every later `borrow()` (the 500 ms `on_vv_event` interval, `on_history_loaded`,
  `on_frame`) panicked, and `cell.replace`/`register_debug_hook` never ran. Fix:
  `on_vv_event()` (like `sync_input_gutter`/`on_change`) is invoked after the
  `with_pile` block, never while a borrow is held. Invariant: **no function that
  borrows `PILE` may be called while a `PILE` borrow is live.**
- Engine reads signals with `get_untracked()` (it is event-driven, not a reactive
  tracking context) so the console stays free of reactive-graph warnings and real
  errors are not buried.
- Programmatic parks (session change / `grew` / `park_bottom` / round-view) run in
  the extracted `step_scrolls` + `park_to_bottom` and set inline
  `scroll-behavior:auto` so they land instantly instead of animating against the
  CSS `smooth` (which would race the 150 ms re-park). The DOM-lag gate runs
  `step_scrolls` too, so parking is never blocked by the Leptos DOM flush.
- `#transcript` padding is `16px 16px 20px` (extra bottom room so the last card's
  shadow/rounded bottom edge is never clipped at the container edge; top stays
  PILE_TOP=16).
- Engine init retry: if `#transcript` is missing on an init tick, the state is
  stashed in a thread-local (`PENDING_INIT`) and retried on a 150 ms timer (≤20
  times, generation-guarded) — the engine no longer depends on a later signal
  change to boot.
- Stall watchdog: if the DOM-lag gate has retried ≥90 frames (~1.5 s) with the
  For DOM still not catching up, a one-shot `console.error` reports the stall and
  `last_panic`, pointing at `__rushiPile()`.
- The rAF loop is event-driven (a frame re-schedules only from the DOM-lag
  gate, scroll, or signal change) — the legacy model; the stall watchdog is the
  safety net for a silently dead loop.

## Build/serve
- [x] `run-webui.sh --rebuild` runs `trunk build` before the server build; the
      default path builds the WASM bundle if `web-leptos/dist` is missing.
- [x] Fresh dist/ hashes verified on the running 8480 server
      (index.html → new .js/.wasm, both 200).

## Diagnostics
- `window.__rushiPile()` (console) → one-line engine-state snapshot:
  `init=… steps=… fold_applied=… compact=n/m events=… cards=… pile_face=…
  pile_open=… kb_open=… summary=… active=… view=… last_panic=…`
- One-shot `[rushi] pile ok: cards=… events=… view=… active=…` console trace on
  the first engine step that sees cards.
