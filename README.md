# rushi-webui

Web front-end for the rushi agent harness. Same Tier-2 position as
`rushi-tui`: a replaceable view over the same event-sourced session
log. The kernel, tools, and hooks are untouched.

> **Local setup note (2026-09):** the TUI is no longer used here —
> the WebUI is the primary front-end. The `rushi-tui` repo (with its
> uncommitted changes) is left as-is and is not part of the build or
> sync path. `sync.sh` and `run-webui.sh` never touch the TUI.

## Architecture

```
Browser (single-file React-free SPA, embedded in the binary)
    │  REST + WebSocket
rushi-web (Rust/axum, single binary via rust-embed)
    │  spawn / signal (process groups)
rushi kernel: claim → assemble → model → parse → route → tools → log
    │
events.jsonl  (append-only event source; the web UI is a viewer)
```

- **Read path**: the UI loads a session's `events.jsonl` over REST,
  then follows a live tail over WebSocket (200 ms poll, newline
  granularity — same guarantee as the TUI's file watcher).
- **Write path**: user messages (`direct` / `steer` / `follow`
  queues), approvals, and rewind events are appended to the log with
  the kernel's `LogLine`-style flock + single-`write(2)` discipline,
  so the kernel's `claim` / `rewind` / `assemble` stages consume them
  exactly as they would TUI-written events.
- **Loop lifecycle**: `POST /start` spawns `[loop] command` (default
  `rushi run <session>`) in its own process group; `POST /stop`
  SIGKILLs the group (loop + in-flight tools).

## Events rendered

All 14 kernel event types: `user_message`, `assistant_message`
(markdown + reasoning + usage → context budget bar), `tool_call`,
`tool_result` (collapsible, error styling), `error`, `ext_status`
(also projected into a live status strip: `loop_phase`,
`model_thinking`, `model_call_context`, …), `compaction_started`,
`compaction_summary`, `compaction_failed`, `context_exhausted`,
`approval_request` (Approve/Deny card → writes `approval`),
`approval`, `rewind` (fork divider in the transcript, and a node in the
history tree — see the rewind plugin), `user_message_retract`
(strikethrough; the target's node carries a `retracted` badge in the tree).

## Goal panel (web-native goal-ext)

The TUI's `goal-ext` row is mirrored as a sidebar panel that reads
and writes the same on-disk layout the kernel hooks use
(`goal.json` pointer + `goal-<id>.json` per-goal files, via the
`goal-state` crate's schema): create / pause / resume / clear / edit
from the browser. The kernel's `model.before` / `run.idle` goal
hooks keep working unchanged.

## Rewind plugin (history tree)

Rewinding to an earlier user message no longer throws anything away: every
branch lives in the same append-only `events.jsonl`, and the abandoned ones
stay visible — and re-enterable — in a history tree. The kernel already had
`rewind` as a first-class event and `POST …/rewind`; this plugin adds the
read-only projection (`GET /api/sessions/{id}/rewind`), the UI, and the guard.

- **The expanded view is a new full-window interface.** `layout-full` is no
  longer the stretched sidebar: `#history-view` replaces it, with a session
  rail on the left (so switching sessions stays full-screen) and the recursive
  round/branch tree filling the rest. The rail is the M7 dispatch view's
  session card — grouped by the session's **working path**, each with its own
  `▶ start` / `■ stop` loop toggle and `…` menu (v0.5.56). One node = one user
  message = one loop
  round; the agent's work folds into the node (summary + event count).
  Abandoned branches are dimmed with a strikethrough summary, the current
  round carries a ring and "here", and compaction boundaries are footnoted.
- **Clicking a node** opens *"Rewind to this point?"* → the active
  conversation resumes from there, everything after it moves to an abandoned
  branch, and the next context assembly ends there (kernel `mode:"on"`). The
  same dialog is reachable from the `⟲` button on every user card and from
  the sidebar panel's active-path rows. Nothing is irreversible, so the
  dialog says so.
- **The plugin also appears in `#plugin-area`** (`goal · essence · rewind`),
  summarizing the tree and offering *open History view*.
- **Rewind is forbidden while the session's loop runs** (an in-flight turn
  would land inside the fresh branch): the tree stays viewable but read-only,
  the dialog's button and the card `⟲` are disabled, and the tooltips/the
  footer say why. No `POST /stop` — wait for idle.
- The projection is proven equal to the kernel's own `active_ranges` over the
  kernel's fixtures (`cargo test -p rushi-web`), and a browser probe
  (`e2e/rewind_probe.py`) asserts the whole flow, including re-entering an
  abandoned branch. Design notes: `docs/rewind-plugin.md`;
  plan: `docs/rewind-plugin-plan.md`.

## Build & run

```sh
# from the repo root (workspace root = rushi/ parent of the kernel)
./run-webui.sh            # builds if needed, serves http://127.0.0.1:8480
```

or via the kernel subcommand (recommended, mirrors `rushi tui`):

```sh
cd rushi && ./target/debug/rushi serve
```

`rushi serve` resolves the web binary like `rushi tui` resolves the
TUI: `[web].binary` in `config.toml` (relative to the config dir) →
side-by-side `rushi-web` next to the kernel binary → `PATH`. It
forwards `[paths] sessions_root` and `[loop] command/args` so the
web server spawns the same loop the TUI would.

Manual flags:

```
rushi-web --host 127.0.0.1 --port 8480 \
  --sessions-root /abs/path/sessions \
  --loop-cmd "/abs/path/rushi run" \
  [--config /abs/path/config.toml] \
  [--ext-dir DIR ...]
```

`--config` pins the kernel `config.toml` for every spawned loop: the
server forwards it as the `CONFIG` env var, so a session's working
directory (its `.cwd` marker, set by the new-session dialog) may be
ANY existing directory — the loop no longer needs a `config.toml` in
it. Resolution order for a pinned value: `--config` → inherited
`$CONFIG` → side-by-side (`<exe>/../config.toml`) → the loop CWD's
`config.toml`; if none resolves, `start` fails up-front with a clear
message instead of spawning a loop that dies at config load.
`run-webui.sh` passes both `--config` and `export CONFIG`, and
`rushi serve` forwards its own resolved config.

When a spawned loop dies abnormally (non-zero exit or signal, not an
intentional stop), the server reads the tail of `sessions/<id>/loop.stderr`
and the WS `loop_status` frame carries it in `detail`; the SPA renders
it inside the "loop stopped unexpectedly" error card.

`--ext-dir` is accepted for forward compatibility (CLI extension
proxying, Phase-3 option A); the web-native equivalents (goal panel +
status strip) are what's wired today.

## Single-binary distribution

The SPA is embedded at compile time with `rust-embed`
(`web-leptos/dist/index.html`), so `rushi-web` is one static binary.
After editing the frontend, re-run `trunk build` (see Dev notes) to
re-embed. In debug builds rust-embed reads `web-leptos/dist/`
from disk, so a `trunk build` is picked up without recompiling
`rushi-web`.

> **Trunk location (2026-09):** the WASM build toolchain lives in the
> repo-root `rushi/.cargo-home` — `trunk` is at
> `rushi/.cargo-home/bin/trunk` (v0.22.0-beta.5, registry pre-cached
> there). `run-webui.sh` sets `CARGO_HOME` to it. Note this is a
> DIFFERENT directory from `rushi-webui/.cargo-home` (which is empty)
> and from the user's default `~/.cargo` — use the launcher, not a
> bare `trunk` on PATH.

## API

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/health` | liveness |
| GET | `/api/sessions` | list sessions (newest first) |
| GET | `/api/sessions/{id}/events` | full event log |
| POST | `/api/sessions/{id}/messages` | `{"content","queue"}` append user message |
| POST | `/api/sessions/{id}/start` | spawn the loop |
| POST | `/api/sessions/{id}/stop` | SIGKILL the loop process group |
| POST | `/api/sessions/{id}/approval` | `{"id","decision"}` answer an approval_request |
| GET | `/api/sessions/{id}/rewind` | the projected history tree (rounds, forks, boundaries, `current_seq`) |
| POST | `/api/sessions/{id}/rewind` | `{"target_seq","mode":"before\|on"}` |
| GET/POST | `/api/sessions/{id}/goal` | read / act on goal state |
| WS | `/ws/sessions/{id}` | history frame + live event frames; inbound `message`/`approval`/`rewind`/`start`/`stop`/`load_earlier` frames; outbound `history` / `history_page` / `event` / `model_stream` / `loop_status` frames |

## Truncated history (v0.5.17)

Long sessions used to ship their entire `events.jsonl` on connect, which
became heavy for large logs (the 13 MB case). The read path is now
windowed:

- **Server** (`rushi-web`): on connect, the `history` frame carries only
  the last page (default 200 lines, `HIST_PAGE`) plus the metadata
  `oldest_line`, `total_lines`, and `has_more`. The inbound
  `load_earlier` command (`{"kind":"load_earlier","before_line":N,"limit":L}`)
  returns an older page via a `history_page` frame with the same fields.
  Line numbers are 1-based and stable because `events.jsonl` is
  append-only. Implemented in `sessions.rs::events_windowed` + the WS
  handler in `main.rs`.
- **Client** (`web-leptos`): `model.rs` holds `hist_oldest_line`,
  `hist_has_more`, and `loading_earlier` signals. `ws.rs` parses the
  metadata on `history`, handles `history_page` (prepending older events
  and refreshing the ctx/tool bookkeeping), and exposes
  `ws::load_earlier()`. `pile::on_history_prepended()` sets a one-step
  `prepending` flag so the scroll engine does not re-arm a bottom-follow
  when the window grows backwards. `transcript.rs` renders a "load
  earlier" pill at the top of the transcript (hidden when the whole log
  is loaded or a round is pinned in view).

Verified by `e2e/truncation_ws.py` (raw-socket WS client, stdlib only)
and the `sessions.rs` unit tests for `events_windowed`.

## Truncation fixes: global round numbering, stuck-button recovery (v0.5.21)

Two regressions the windowed history introduced:

- **Round chips showed only the window's rounds** (e.g. 1–2 chips for a
  50-round session, numbered from 1). The `history`/`history_page`
  frames now carry `total_rounds` — a cheap whole-file scan for
  `"user_message"` lines in `events_windowed` (no JSON parse;
  `user_message_retract` does not match the quoted pattern). The
  client labels chips with their global round number
  (`total_rounds - window_rounds + i + 1`; with the full log loaded
  this collapses to the old 1-based numbering) and shows a
  leftmost `⋯ N` chip — N rounds not yet loaded — that triggers
  `load_earlier`, so the chip row fills in as pages load.
- **The "load earlier" pill could stick disabled**: `loading_earlier`
  was only cleared by a `history_page` frame, so a dead socket, an
  `error` reply, or a pre-v0.5.17 server left it latched. Now: a 5 s
  watchdog in `ws::load_earlier()` releases the flag and sets
  `earlier_failed` (the pill shows "earlier failed · click to retry");
  `error` frames clear it; and the socket **auto-reconnects** ~1 s
  after `on_close` (session-name + `is_open()` guards prevent a
  switched-away or mid-handshake socket from double-connecting).
  Successful pages show their size in the pill label
  ("↑ N earlier · M loaded").

Rebuilt via `trunk` (see Dev notes) — the new `dist/` is picked up
by the running debug server, no server restart needed for the
frontend; the `total_rounds` field does need the new
`rushi-web` binary.

## UI-freeze fix + regression test (v0.5.23)

**The bug.** During model streaming the UI could freeze: every
`model_stream` delta ran the full reactive pipeline per message
(`live_text.update` + streaming flag + pile `on_change()`), and each
one re-parsed the whole growing markdown document and re-rendered the
streaming card. A fast local model emitting dozens of deltas per
frame saturated the main thread — the UI was responsive only between
bursts, and a long document made each delta more expensive (O(n)
re-parse), so it got worse the longer the reply ran.

**The fix (P0-1: delta coalescing).** Deltas no longer run the
pipeline per message. `model_stream` text/reasoning chunks are
appended to thread-local pending buffers
(`PENDING_TEXT` / `PENDING_REASONING` in `ws.rs`); one
`FLUSH_SCHEDULED` flag ensures at most ONE flush is scheduled per
frame (`schedule_delta_flush`, rAF-throttled). The flush appends the
whole pending batch with a single `set()`, so the expensive
re-parse + DOM cycle + engine step happens once per frame no matter
how many deltas arrived. The final message (or session switch /
stream end) drains the buffers and clears the flag
(`drop_pending_deltas`) so a queued flush can never append into a
new session's card. P0-3 (scroll-only pile round detection) and
P1-4 (bounded `leak_timeout` retention) ship in the same change.

**Regression test: `?test=freeze`.** Opening the app with
`?test=freeze` runs the test ~1.5 s after mount: it seeds the
streaming card with a large markdown document, then drives a flood
on a 16 ms timer chain (rAF never fires in the headless shell the
e2e runner uses; a real browser gets the same cadence). The
default mode pushes each tick's 60-delta batch into the production
pending buffers and invokes the coalesced flush once — the real
code path. A 0 ms timer round-trip queued behind each tick
measures how late timers actually fire (a saturated main thread
delays them into the hundreds of ms; a synthetic `el.click()`
handler would run inline in the same task and measure nothing).
Verdict: **PASS** when the worst tick gap < 100 ms AND the worst
timer round-trip < 150 ms. The result goes to
`window.__rushiFreezeResult`, the console, a visible badge, the
`<title>`, and persistent `<html data-freeze-verdict/-gap/-timer>`
attributes (the badge auto-dismisses; the attributes outlive it).

**The negative control: `&flood=raw`.** Appending `&flood=raw`
emulates the PRE-fix behaviour: each of the 60 per-tick deltas is
delivered as its own timer task (pre-fix, each WS message was its
own task, so Leptos flushed between deltas) and the raw mode stops
early (`FLOOD_EARLY_STOP_FRAMES`) once it has proven the failure.
This control is **expected to FAIL** (observed: ~590 ms gap /
timer vs the 100/150 ms gates) — that is how the test proves it
discriminates, not just runs.

Headless runner (CI/e2e):

```sh
e2e/freeze_regress.sh 8480 both     # default (expect PASS) + raw (expect FAIL)
```

It drives the Playwright headless Chromium shell over CDP in real
time and reads the `<html data-freeze-*>` verdict attributes.
Exit 0 = both verdicts match expectation, 1 = regression or lost
discrimination, 2 = inconclusive.

## Mobile sticky-bottom fix: slow pull-up release

**The bug.** On a phone, while the transcript was parked at the
bottom (sticky follow active), a SLOW pull-up to read history could
not break the bottom lock — the viewport got pulled back to the
bottom on every engine step. Only a very fast flick managed to
escape.

**Root cause.** The v0.5.10 touch-intent detector compared each
`touchmove` to the previous event with a 0.5 px dead zone. At the
60–120 Hz touch sampling cadence, a gentle pull moves far under 0.5
px per event, so the up direction never latched (`input_up` stayed
false) and the sticky gate's release rule (`recent_input &&
input_up`) never fired. With the flag still set, the tight-follow
pin re-snapped the viewport to the bottom on every step — the
"bottom lock". A fast flick exceeds 0.5 px per event and latched,
which is why only fast drags worked.

**The fix (code comments tagged v0.5.24 / v0.5.24.1,
`web-leptos/src/pile.rs`).**

- Direction latches on CUMULATIVE drift from a gesture anchor
  (`TOUCH_INTENT_DRIFT_PX` = 8 px hysteresis, re-anchored at every
  latch), so a slow pull registers after ~8 px of travel regardless
  of event cadence.
- v0.5.24.1: gesture boundaries come from the real
  `touchstart` / `touchend` / `touchcancel` events on the
  transcript — the anchor is set at the touchdown and survives
  mid-gesture pauses. (The v0.5.24 interim heuristic, `TOUCH_GESTURE_GAP_MS`
  = 250 ms between `touchmove` events, re-anchored on any pause
  longer than 250 ms, so a hesitant slow pull kept losing its
  accumulated drift and could never reach the latch — the same
  symptom on the phone.)
- v0.5.24.1: while a finger is on the transcript (`touch_active`)
  the flat step suspends every programmatic `scrollTop` write —
  the tight-follow pin, the settle pull, the `park_bottom`
  consumption and the 150 ms re-park timer. The browser's native
  pan owns the viewport mid-gesture, so the app no longer fights
  the finger ("yanked back to the bottom" felt during the pull).
- READING MODE (v0.5.24.1, gate corrected in v0.5.24.2):
  being far above the bottom — not the input-event latch — decides
  whether the user is reading. The positional `reading` latch
  (latched when a release leaves the viewport > 80 px above the
  bottom; self-cleared when the viewport returns within 80 px)
  suppresses the v0.5.19 tier-1 round-boundary re-arms (`stream
  start` / `round end` / new-event proximity re-arm) and
  `rearm:passive-clamp`, so a reader is not snapped back at every
  model call / round end. It clears when the user pulls back down
  (`rearm:down`), on a new sent message (`rearm:new-msg` — sending
  means watching), or on a session switch.
- v0.5.24.2: the re-arm gates must key on the positional `reading`
  latch, NOT the sticky `input_up` latch. v0.5.24.1 gated the tier-1
  and passive-clamp re-arms on `!input_up`; `input_up` is latched by
  any up-input (including a single stray wheel/touch tick at the
  bottom) and stays latched until an explicit down-pull, so after a
  round end the re-arms were permanently suppressed — the viewport
  got stranded at the top of the just-finalized card and the follow
  died ("streaming card finishes, view jumps back to the card top").
  The gates now use `!reading` only (self-cleared at the bottom), so
  a stray tick at the bottom re-arms the follow on the next
  round-boundary event, while a genuine up-pull (d > 80) keeps it
  off.
- v0.5.38: the round-end wedge's REAL trigger. When a round's final
  `assistant_message` lands, the in-flight `.ev-streaming` card is
  unmounted and the canonical card mounts in its slot. That card-swap
  changes the content height across frames, so `sync_stick`'s
  one-frame `shrink` measure under-counts it and the residual reads as
  an "unexplained" upward viewport move — misfiring
  `release:unexplained`, which released the follow AND latched the
  positional `reading` flag (the viewport sat >80px above the new
  bottom). Both then blocked every round-end re-arm, stranding the
  viewport at the top of the just-finalized card with no follow. The
  fix marks the finalization frame (`finalize_t`, set in `step_full`
  when `streaming` goes true→false) and, within a 400 ms settle
  window after it, treats an upward delta that `shrink` can't explain
  as a PASSIVE finalization layout shift (route to the passive-clamp
  branch, re-arm if released) instead of a user scroll; the settle
  pull then snaps to the true bottom. Reader protection is
  untouched: a reader actually far above the bottom (`reading` latched
  by their own scroll) is still not re-armed at round end.
- v0.5.39: fixed the "yanked to the bottom while I still had ~2 lines to go"
  regression. The 80 px live-edge band is too coarse to distinguish a watcher
  at the bottom (d ~ 0, should keep following) from a reader parked a few
  lines above it (d ~ 40, must NOT be snapped down). The v0.5.24.2
  `!reading` gates let the latter get teleported to the bottom by rearm:down
  (a down tick landing inside the band) or by round-end re-arms. Fix: tighten
  every AUTO re-arm to a one-line live edge, LIVE_EDGE_TIGHT = 24 px, in
  pile.rs:
  - rearm:down now fires only at d <= 24; a down-scroll that stops in the
    24-80 px band no longer snaps to the bottom, the user's own scroll
    carries them the rest of the way and re-arms when they actually reach
    the bottom.
  - the tier-1 round-boundary re-arms (rearm:stream-start / rearm:round-end /
    rearm:new-event) gain an outer gate `!reading && prev_dist <= 24`.
  - rearm:passive-clamp gains the same gate, keyed on the PRE-shrink
    resting distance st.prev_dist (NOT the post-shrink dist, which is
    transiently the whole shrink amount and would wedge the v0.5.38
    round-end follow back off).
  A bottom watcher (rested ~0) still re-arms on round end; a reader parked
  >24 px above the bottom is left where they are; far-up readers
  (d > 80, reading latched) remain protected as before. Verified headless
  via the local e2e/liveedge_probe.py probe (not checked in; see Dev notes)
  — rearm:down at bottom, shrink-while-watching, released-bottom shrink re-arm
  all pass; the 40 px parked-reader case is
  device-verified by scrolling up ~2 lines, letting a round end, and
  checking __rushiPile(): expect stick=false, reading=false,
  prev_dist > 24.
- Multi-touch (pinch) is not scroll intent: it abandons the anchor
  so the next single-touch gesture starts clean.
- The v0.5.19 proximity re-arm (a new event arriving while the
  viewport rested within 80 px of the bottom) now requires
  `!reading`, so a user who pulled up to read is not snapped back to
  the bottom by the next event while actually far above the bottom.

**Regression test: `e2e/touch_regress.sh [port]`.** Drives full
Chromium in `--headless=new` mode over CDP (unlike the headless
shell used by the freeze test, `--headless=new` fires
`requestAnimationFrame`, which the pile engine's step loop needs).
It opens a session that has history (clicking sidebar items until one
loads cards, since the first item may be an empty session), waits for
the cards to land and the viewport to park at the bottom (`stick=true`
in `__rushiPile()`), then dispatches full synthetic gestures
(touchstart → touchmoves → touchend) on `#transcript`:
1. steady slow pull up (25 × 0.4 px) → `stick=false`,
   `release:up-input`
2. slow pull down → `stick=true`, `rearm:down`
3. HESITANT pull up from the stuck state (3 × 4 px with 400 ms
   pauses — each pause longer than the old 250 ms gap heuristic,
   the exact real-phone failure shape) → `stick=false`,
   `release:up-input`
4. final pull down → `stick=true`, `rearm:down`
5. round-end shrink: stray up-tick at the bottom (releases the
   follow, `reading` stays false — the live-edge band, d ≤ 80),
   then hide the last card (content shrink = the round-end
   finalize shape) → the passive clamp must fire
   `rearm:passive-clamp` and restore `stick=true` (the v0.5.24.2
   regression guard: under the old `!input_up` gates the follow
   died and the viewport sat at the card top)

Exit 0 = all scenarios as expected, 1 = regression, 2 =
inconclusive (no CDP target, no initialized pile, or no session
with content).

**Live round-boundary check: the local `e2e/liveedge_probe.py [port]` probe (gitignored, not checked in — see Dev notes) or a real
session.** The tier-1 round-end re-arm and the v0.5.38 finalization
suppression need a *live* round (a real `assistant_message` finalizing),
so the headless `touch_regress` can't exercise them. `liveedge_probe.py`
drives full Chromium `--headless=new` over CDP and covers the cases
that *are* reachable headlessly — rearm:down at the true bottom, a
round-end shrink while watching (settle pull), and a released-follow
round-end shrink re-arm (rearm:passive-clamp, the v0.5.38 guard) —
plus a printed device-verify note. The live round-boundary behaviour
on a *real* round, and the v0.5.39 "reader parked ~2 lines up is not
snapped" case, are verified on a device: drive one round in a scratch
session and read `__rushiPile()` at the round end:
(a) plain watch → `stick=true`, `dist≈0` (the v0.5.38 fix: the
finalize card-swap no longer misfires `release:unexplained`);
(b) a single stray up-tick at the bottom, then round end → `stick`
returns to true via `rearm:passive-clamp` (the v0.5.24.2 fix);
(c) a reader parked ~2 lines (~40 px, `prev_dist > 24`, `reading`
false) → NOT snapped back by the round end (`stick` stays false,
`prev_dist` stays > 24); a pre-v0.5.39 build shows `stick=true` there;
(d) a genuine pull-up mid-round (d > 80, `reading` latched) → NOT
snapped back, then re-arms on pull-back-down (`rearm:down`).
`__rushiPile()` reports `input_up=` / `reading=` / `prev_dist=` /
`touch_active=` for triage.

## Dev notes

- Local CDP test probes: `e2e/*.py` / `e2e/*.sh` (touch_regress,
  freeze_regress, liveedge_probe, earlier_anchor, earlier_flicker,
  truncation_ws) drive a real headless Chromium over CDP against a
  running rushi-web. They are developer regression tools, kept
  LOCAL-ONLY — `e2e/` is gitignored and not part of the repo, so the
  `e2e/...` paths cited above resolve only on a dev machine that has
  the probes checked out.

- Mobile Safari: the input row is kept above Safari's bottom URL bar /
  home indicator with `height: 100dvh` (no-JS fallback), a
  `visualViewport` resize listener that sizes the body to the live
  visible viewport, and `env(safe-area-inset-bottom)` padding under
  `viewport-fit=cover`.
- Local model: `config.toml [model] api = "responses"` pointing at
  the sglang server (`http://127.0.0.1:30000`), key env
  `LLAMA_API_KEY` (any value).
- Tool/hook resolution: the kernel resolves stage binaries next to
  its own exe and looks up hooks/tools on `PATH` (the `bin/`
  symlink farm in the workspace root), so `rushi serve` inherits the
  launcher's environment.
- Frontend build: `trunk build` in `web-leptos/` (toolchain in the repo-root
  `.cargo-home`, see the Trunk note above) writes `web-leptos/dist/`, the
  embedded artifact. In debug mode `rushi-web` reads it from disk, so no
  server recompile after a `trunk build`. The legacy no-JS `web/` SPA is
  superseded by the Leptos build.
