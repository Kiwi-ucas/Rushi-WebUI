# Rewind plugin — the history tree

Status: **implemented** (P1–P7 of `rewind-plugin-plan.md`). UI text is English.

The rewind plugin lets a user rewind a conversation to any earlier user
message **without losing anything**: every branch stays in the same
append-only `events.jsonl`, and the abandoned branches remain visible and
re-enterable in the history tree.

It is a plugin in the strict sense of `plugin-authoring-rules.md`: one server
module, one client module, one API surface, one `AppState` slice. The kernel
is untouched — rewind was already a first-class kernel event.

```
server   bin/rushi-web/src/rewind.rs     GET  /api/sessions/{id}/rewind  (projection)
                                         POST /api/sessions/{id}/rewind  (write, pre-existing)
client   web-leptos/src/rewind.rs        History view + dialog + sidebar panel + card button
         web-leptos/src/model.rs         RewindTree / RewindNode / RewindMarker / RewindBoundary
         web-leptos/src/api.rs           load_rewind_tree / post_rewind
         web-leptos/src/plugins.rs       registry entry (goal · essence · rewind)
         web-leptos/src/lib.rs           mount + the single tree-fetch effect
         web-leptos/src/transcript.rs    the `⟲` card button
         web-leptos/style.css            #history-view / #hist-* / .rw-* / dialog
test     bin/rushi-web/src/rewind.rs     unit tests (kernel fixtures + ported active_ranges)
         e2e/rewind_probe.py             71 browser assertions over three fixtures
```

## 1. Kernel semantics this view mirrors

`rushi/crates/rushi/src/rewind.rs` is the truth. A `rewind` event at log line
`S` with `target_seq` `T` and `mode ∈ {before, on}` states:

```
context(S) == context(T_eff),   T_eff = T      (mode "on")
                                      = T - 1  (mode "before")
```

The events in `(T_eff, S)` leave the active path — they are **masked**, not
deleted. `active_ranges(total, rewinds)` computes the kept ranges recursively,
so a rewind inside an already-masked branch re-opens that branch's whole path.
`bin/claim` then either continues a loop there or settles the session to idle
when the target is a user message.

Line numbers are the kernel's `seq`: **one per non-empty log line**, blank
lines skipped, malformed lines still owning their number. The webui's
`events_windowed` (`oldest_line`) uses the same counting, which is what lets a
rendered card compute its own `target_seq` as `hist_oldest_line + index`.

## 2. The server projection (P1)

`bin/rushi-web/src/rewind.rs` is a **pure function over the event list**
(`build(session, &[Value])`), so it is fixture-testable with no I/O. One scan,
three passes:

1. **Structural scan** — `user_message` opens a round node (`seq`, `round`
   index, `ts`, content, `id`); a valid `rewind` becomes a marker (same
   validation rules as the kernel's `parse_rewind_event`: integer
   `target_seq >= 1`, known `mode`, `target < seq`, malformed ⇒ ignored);
   `compaction_summary` yields a boundary; `user_message_retract` names the
   message id it retracts. Everything else is noise (but counts toward the
   round's folded `events`).
2. **Cursor + parent map** — the fork structure:
   ```
   cursor = None
   user_message U      -> parent[U] = cursor; cursor = U
   rewind(S, T, mode)  -> cursor = round_at(T_eff)
   ```
   The second time a round gains a child, that is the fork: the next round
   attaches to the target, and the previously-next round stays as the
   abandoned sibling.
3. **Active path & states** — `active` = the final cursor's parent chain;
   `current` = the final cursor; `settled` = the log's last structural event
   is a rewind marker.

Wire format (`GET /api/sessions/{id}/rewind`, recursive `children`):

```jsonc
{ "session": "id", "total_events": 142, "total_rounds": 4,
  "roots": [ { "seq": 12, "round": 3, "ts": "…",
               "summary": "first ~60 chars of the user message",
               "events": 7,             // non-ext_status lines folded in
               "state": "active",       // "active" | "abandoned"
               "current": false, "retracted": false,
               "children": [ … ] } ],
  "rewinds":   [ { "seq": 40, "target_seq": 12, "mode": "on" } ],
  "boundaries":[ { "seq": 61, "first_kept_seq": 61 } ],
  "current_seq": 58, "pending_from": 58, "settled": false }
```

**Correctness.** The projection's active set is asserted against a *ported*
20-line copy of the kernel's `active_ranges` over the kernel's own fixtures
(`nested_forks_mask_the_abandoned_branch`,
`reentering_a_branch_rebuilds_its_path`, `deep_chains_follow_the_nested_targets`,
`rewind_at_log_end_has_no_continuation`, `before_mode_excludes_the_target`),
plus a synthetic multi-fork session. A divergence fails `cargo test -p rushi-web`.
The port (rather than a `rushi-common` dependency) keeps the webui binary's
build independent of the kernel crate, per the strict-separation rule.

## 3. The client

**One data source.** `rewind.rs::register_tree_effect` is the single fetch:
keyed on `active_session` and `rewind_gen`. `ws.rs` bumps `rewind_gen` only on
an incoming `user_message` or `rewind` frame — the only structure-changing
event types — so a streaming session does not refetch per event. Both surfaces
read `state.rewind_tree`.

**The History view** replaces the old `layout-full` dispatch placeholder: the
`#history-view` sibling of `#main`, mounted only when `layout == "full"`, with
`#sidebar` and `#main` out of the layout entirely.

```
#history-view
├─ #hist-top    ◀ back to chat · HISTORY — <session> · loop lamp · theme
├─ #hist-body
│  ├─ #hist-rail   sessions grouped by project (compact cards; click = select)
│  └─ #hist-tree   the scroller
│     ├─ .rw-legend            active · abandoned (n) · n rewinds
│     ├─ .rw-node (recursive)  ● round i · summary · N events · time · state
│     │  └─ .rw-kids           guide line; abandoned = dimmed + strikethrough
│     └─ .rw-boundaries        compaction floor footnotes
└─ #hist-foot   "click a node to rewind · disabled while the loop runs"
```

Node states are the projection's: **active**, **abandoned** (dimmed,
strikethrough summary), **current** (ring + `here`), plus the boundary
footnote. Tooltips carry round, time, folded event count and state — and, when
disabled, the reason.

**The sidebar panel** (`#plugin-area` → `rewind`) is the C1 entry: it proves
the plugin is loaded, summarizes the tree (`4 rounds · 3 active · 1 abandoned`),
lists the active path (each row opens the same dialog) and offers
*open History view*. The 30 % panel cap stays; the full tree is the expanded
view, where the cap does not apply.

**The confirm dialog** is one component for all three entry points (tree node,
panel row, card button): title **"Rewind to this point?"**, the body

> The active conversation resumes from here. Everything after it moves to an
> abandoned branch — it stays in the history tree, and you can rewind back to
> it later.

and **Cancel** / **Rewind**. There is no "irreversible" warning on purpose:
nothing is lost, and a second rewind re-enters the abandoned branch. Confirm
writes `post_rewind(session, seq, "on")` — the target message stays the active
tail, everything after it is abandoned, and the next context assembly ends
there. Errors keep the dialog open with a red line.

**The card button** (`⟲`, bottom-right of every `.ev-user` card) targets the
card's own log line (`hist_oldest_line + card index`) and opens the same
dialog.

## 4. Rewind is forbidden while the loop runs (decision 5)

An in-flight turn would land inside the freshly-created branch (the kernel
clears and rebases its pending lists at the next claim), so the plugin refuses
the action instead of racing it — no `POST /stop`, just wait for idle.

The guard is client-side, at all three entry points, from two signals:
`loop_running` (the 4 s `/api/sessions/{id}/loop` poll) and `looping_sessions`
(the server's truth: in-memory loop pids plus every session's `loop.pid`
liveness, resynced from `/api/loops` on mount and every 10 s, and from the
WS `loops` frame on connect).

When locked:

* the tree stays **viewable** but every node carries `.locked` (no hover
  affordance, `cursor: default`), a click opens nothing, and the tooltip and
  the footer say why;
* the top bar's loop lamp breathes;
* the dialog's **Rewind** button is disabled;
* the card `⟲` buttons are disabled with the same tooltip.

The kernel's rewind semantics are unchanged: it remains a first-class event.

## 5. Edge cases

| Case | Behaviour |
|---|---|
| Compaction floor (kernel rule I4) | A node older than a boundary's `first_kept_seq` is shown; rewinding there degrades to the boundary. The `.rw-boundaries` footnote states it. |
| Freshly sent, not-yet-claimed message | A `rewind` with nothing after it settles at the target (`settled: true`); the tree moves `current` there. |
| Retracted message | Non-structural: the node keeps its place, the badge shows. |
| TUI-written targets (`tui_pick`, a `tool_result` line) | `round_at(T_eff)` resolves to the round containing that line — a display simplification. This plugin always writes a user message's own line. |
| Multi-client | Every client refetches on the WS `rewind` frame. |
| Window vs. full log | The tree is the server's full-log projection; the card button uses `hist_oldest_line + index` for rendered cards only. |
| No session / no rounds | "select a session" / "no rounds yet". |
| Performance | One file read + parse per structure-changing event; a multi-MB log is a one-off parse. |

## 6. Verification

* `cargo test -p rushi-web` — the projection against the kernel fixtures
  (42 tests green).
* `trunk build` — the Leptos CSR bundle.
* `python3 e2e/rewind_probe.py [port]` — 71 browser assertions over three
  fixture sessions built by the probe (`/tmp/rw-e2e/sessions/rewindprobe`,
  forked; `rewindprobe2`, a second session; `rewindprobe3`, no round at all): the rebuilt expanded view (layout, rail,
  scroller), the rail switching sessions in-view, the recursive tree
  (abandoned / current / retracted / boundary / tooltips / legend), light +
  dark theme, the dialog copy and its English text, node-click ⇒
  `{"type":"rewind","target_seq":6,"mode":"on"}` appended and the marker
  moving live, **case C** (re-entering the abandoned branch — the log stays
  append-only), the card button (present, inert on the current tail, same
  dialog), the `#plugin-area` entry, and the loop-running guard (with a live
  `loop.pid`: every node locked, no dialog, footer explains, card buttons
  disabled).
