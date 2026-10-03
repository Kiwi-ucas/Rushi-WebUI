# Rewind plugin — the history tree

Status: **implemented** (P1–P7, the v0.5.56b/v0.5.57 additions, and
**Style B — the flow scene**, v0.5.63–v0.5.66, of
`rewind-plugin-plan.md`; §10 there is the plan, §10.8 the as-built record).
UI text is English.

The rewind plugin lets a user rewind a conversation to any earlier user
message **without losing anything**: every branch stays in the same
append-only `events.jsonl`, and the abandoned branches remain visible and
re-enterable in the history tree.

It is a plugin in the strict sense of `plugin-authoring-rules.md`: one server
module, one client module, one API surface, one `AppState` slice. The kernel
is untouched — rewind was already a first-class kernel event.

```
server   bin/rushi-web/src/rewind.rs     GET  /api/sessions/{id}/rewind  (projection)
                                         GET  /api/sessions/{id}/rewind/node/{seq}  (one round)
                                         POST /api/sessions/{id}/rewind  (write, pre-existing)
client   web-leptos/src/rewind.rs        History view (both styles) + dialog + sidebar panel + card button
         web-leptos/src/model.rs         RewindTree / RewindNode / RewindMarker / RewindBoundary
         web-leptos/src/api.rs           load_rewind_tree / load_rewind_detail / post_rewind
                                         (both rewind reads go through parse_deep)
         web-leptos/src/plugins.rs       registry entry (goal · essence · rewind)
         web-leptos/src/lib.rs           mount + the single tree-fetch effect
         web-leptos/src/transcript.rs    the `⟲` card button
         web-leptos/style.css            #history-view / #hist-* / .rw-* / .rw3-* / dialog
test     bin/rushi-web/src/rewind.rs     unit tests (kernel fixtures + ported active_ranges)
         e2e/rewind_probe.py             browser assertions over four fixtures (Style A)
         e2e/flow_style_b_probe.py       browser assertions over the live sessions (Style B)
         e2e/flow_check.py               the flow projection, session by session (HTTP)
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
│  ├─ #hist-rail   the dispatch-view session cards, one group per working
│  │               path (`.dispatch-group` / `.dispatch-card`); click = select
│     ├─ .rw-legend            active · abandoned (n) · n rewinds
│     ├─ .rw-node (recursive)  ● round i · summary · N events · time · state
│     │  └─ .rw-kids           guide line; abandoned = dimmed + strikethrough
│     └─ .rw-boundaries        compaction floor footnotes
└─ #hist-foot   "click a node to rewind · disabled while the loop runs"
```

The rail is **the M7 dispatch view's card, brought back inside the expanded
view** (v0.5.56): `ui::session_group_head` (the working directory + card
count, full path in the title) and `ui::session_card` — the session name, its
last-output time, **its own loop toggle** (`▶ start` / `■ stop` over REST, for
any session, live from `/api/loops`) and the `…` rename/delete menu. The
grouping key is the session's `cwd` marker (`model::dispatch_groups`), so the
sessions fall under one head per project, with "(no project)" for the ones
without a marker; the group heads are ordered by the active sort mode.

One behaviour differs from the dispatch view, by design (C2): the cards are
called with `stay = true`, so clicking one switches the tree's session in
place instead of dropping back to the `split` layout. The loop toggle is the
dispatch view's: it starts/stops **that** session's loop even when it is not
the active one, and it stays available while the tree is read-only (the guard
only forbids the *rewind*, not the loop control).

**The group head (v0.5.57).** A group is one working path, so two projects can
share a basename (`…/rushi` and `…/rushi/rushi`). The head therefore shows the
basename and carries the **full path as the basename's own tooltip**
(`.dispatch-group-name[title]`), and it can be renamed: hovering the head
reveals `✎`, which swaps the label for a compact input (Enter/blur commits,
Escape cancels, empty clears). The rename is a **display-only alias** — a
`{ "<full path>": "<label>" }` map in localStorage
(`rushi-project-labels`), next to every other UI preference
(`rushi-layout`, `rushi-sort-mode`, …). No server call, no write into the
session directory: the session's `cwd` marker — the real working path — is
never touched, and the tooltip keeps showing it. M7 renders a basename
uppercase; a custom alias is shown as typed (`.dispatch-group-name.custom`).
Because the head is shared, the sidebar dispatch view gets all of this too —
this half is **general webui UI, not plugin code**: the plugin only calls
`ui::session_group_head`.

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

## 3b. The second style: the flow scene (Style B, v0.5.63–v0.5.66)

The History view has two styles, switched from the top bar
(`#hist-style` in `#hist-top`). They are **one tree read two ways**: same
fetch (`register_tree_effect`), same cache (`state.rewind_tree`), same
invalidation (`rewind_gen`). Style B simply ignores nothing and Style A
ignores `flow`; the default is the list (**D2**), and the choice is persisted
under `rushi-rw-view` — a plugin-owned key.

```
[ list | flow ]   ← #hist-style, persisted in localStorage 'rushi-rw-view'
                   list = §3 above (full height, recursive)
                   flow = #rw-split → 1 : 2  (D3 — flow only)
                          ├─ #rw-detail   the selected round, in full (D8)
                          └─ #rw-flow     the scene (D1, D4–D7, D9)
```

### 3b.1 The projection (server, pure, tested)

`rewind::flow(&tree)` turns the same `RewindTree` into a flat, render-ready
scene. Nothing about it is client-side, because the client crate cannot be
unit-tested (`cargo test -p rushi-web-ui` does not build for the host):

* **the main line** — the longest chain over the whole tree, ties to the
  chain holding the current node, then leftmost (D1). An abandoned branch can
  therefore *be* the line, and the current node then sits on a fork;
* `x` = the column along the line, `lane` = the row offset (alternating
  ±1 on each side so two branches never collide), `cols`/`lanes`/`main_len`
  for the CSS;
* `edges` — one per parent→child step, `main` for the ones on the line;
* `branches` — one per off-line chain, with the column it hinges at (and
  `lane 0` is the line itself, which the runs already draw);
* `state` — `active` / `abandoned`, straight from the kernel's mask, so the
  live path is lit and the abandoned branch is dim (**D1**).

`GET /api/sessions/{id}/rewind/node/{seq}` (B2) serves **one round in full**
— the verbatim user message, the event counts, the badges, never a summary —
and 404s for a seq that is not a round (**D8**). It is fetched on selection
and preferred over the node in the tree, so the panel is exact even while the
detail request is in flight.

### 3b.2 The scene

* one `button.rw3-node` per round: `data-seq/x/lane/main/current`, a dot and
  the round number, a native `title` tooltip — **no text in the graph**
  (**D6**); a dot only ever **selects**, it never rewinds;
* the line as `.rw3-seg.main` runs, a fork as `.rw3-elbow` (at the child's
  lane) plus a `.rw3-hinge` (bridging the parent's lane to the child's —
  omitted when a nested fork shares its lane, which would be a zero-height
  hinge);
* every run a dot is *fed* by is lit or dim by the **child's** state;
* **auto-fit** (D4): the column pitch is `clamp(26px, 100cqw / cols - 4px,
  64px)`, so a short line fills the panel and a long one gets the floor and
  pans; the lane pitch is a **percentage of the scene's own height**
  (`calc(50% / (lanes + 0.5))`), so every lane always fits one screen;
* `#rw-flow-scroll` scrolls **x only** (`overflow-x: auto`,
  `overflow-y: hidden`, scrollbar hidden): drag-to-pan on empty space (never
  on a dot) and **the wheel's vertical delta** pan along the line, while a
  horizontal delta — or shift+wheel — stays the browser's own x-scroll
  (**D7**: one gesture, one meaning, never doubled);
* **the light** (D9): `#rw-flow-track` carries the `perspective`, the grid
  `preserve-3d` (the *scroller* must not be the perspective element — an
  `overflow` ancestor flattens 3D), one translucent `.rw3-ribbon` per forking
  branch, and one sticky `.rw3-sheen` band pinned to the panel that the scene
  slides under. The dots and runs are never transformed, so labels stay
  upright. Where the browser has it, `animation-timeline: view(x)` drives the
  turn on the compositor; otherwise (Firefox) — and in the CSS either way —
  the turn is `rotateX` of a pure `calc()` over **plane numbers**:

  ```css
  --turn: clamp(-1, (var(--bx) - var(--rw-scroll) - var(--halfpw)) / var(--denom), 1);
  transform: rotateX(calc(var(--turn) * 62deg));
  ```

  `--bx` (the ribbon's centre), `--halfpw` and `--denom` are written at
  **layout** time (mount, tree change, selection, `resize`);
  `rewind::mark_scroll` writes the **one** value a scroll frame updates
  (`--rw-scroll`), from `on:wheel`, the drag and `on:scroll` — so nothing
  reads layout while scrolling. `prefers-reduced-motion: reduce` sets
  `transform: none`: a flat, static, readable scene.

### 3b.3 The panel

`#rw-detail` shows the selected round: its number, time, event count, badges
(`here` / `off the path` / `retracted` / a compaction boundary / what a
restore would load), and the user's message **verbatim** with `pre-wrap`. The
`⟲ Rewind to this point` button is the *only* rewind entry point here (as in
Style A, §4): it is disabled on the current round, when the loop runs, or
when the target is blocked, and otherwise calls the same `request_rewind` →
the same confirm dialog (§3's one confirm point). Entering the style
pre-selects the **current** round (**D5**), so the panel is never empty and
the button explains itself by being disabled.

### 3b.4 Limits (measured, see §10.8 of the plan)

* A scene that **fits** its panel has nothing to pan, so its branches keep
  the static turn their position under the light implies (a short, forked
  tree is the visible case). Long sessions pan and turn as designed.
* ~~**Style A's** recursive list renders one DOM level per round and traps the
  wasm stack past roughly 32–80 rounds.~~ **Fixed (v0.5.67)**: a chain is no
  longer recursive — a node with a single child continues as a sibling, so the
  recursion depth is the number of *forks*, not rounds. Measured after: all
  eight live sessions render in the list style, `Webui` **117** nodes and
  `alpha` **82**, zero traps (pre-fix: 6 nodes each, 5 and 8 traps). A run is
  now one flat column (one DOM level per fork). See plan §10.9.
* Both rewind reads parse **past serde_json's 128-level limit**
  (`api::parse_deep`, `unbounded_depth`): the tree nests one `children`
  array per round, so without it every long session failed the parse and
  History sat on "loading…" (v0.5.65 fixed this for four of the eight
  sessions on this host).

### 3b.5 The default style

The History view opens in **`flow`** by default since v0.5.67 (user decision;
it supersedes the earlier "list is the default"). `list` stays one click away
and persisted under `rushi-rw-view`; Style A's probe pins that key explicitly
so it always tests the style it means to test.

### 3b.6 The cone: how the flow scene is laid out (v0.5.68 → v0.5.70)

The session's history stays the horizontal **axis** — the trunk, which never
moves — and every **forking branch** is a straight **ray** leaving its
parent's bead. History length grows along x, the fork count spreads *away*
from the line over a cone surface, and the two stop competing for screen
rows. A branch is one element, one line and its beads; there is no ribbon and
no elbow, and every connector in the scene is straight (the user's rule:
"every connector must be a straight line with no bends", "no background
ribbon per branch").

* **The ray.** A branch's container is a zero-size point **on its parent's
  bead**; it holds one bar (`hypot`/`atan2` over the container's own span and
  slope) and the beads that branch owns, each one column right and one `q`
  out. `q = (axis − margin − band/2) / longest branch` is the auto-fit slope:
  the longest branch's last bead lands on the panel's top edge, and every
  other branch is steeper-looking or shallower for free — one slope for the
  whole cone, so branches never cross. The x step stays one column per round
  (D-cone-3), so a branch never becomes a second time axis.
* **The angle.** `theta = (slot − align) × 30° + (scroll − rest) × (360°/1.5·width)`,
  clamped to ±60°: three slots are inside the arc, the rest *park* at its ends
  — pushed back in depth and faded, so they read as a queue and every branch
  takes its turn at the front as the scene pans. Scrolling moves exactly one
  value (`--rw-scroll`); everything else is written at layout time.
* **A fork off a branch nests.** When a branch's parent is itself on a branch,
  its container is rendered *inside* the parent's container (D-cone-6), at
  `−po·q` along the parent's plane, so its line starts on the parent's bead
  and it fans **relative to its parent** (`slot·30°`, with no second phase —
  D-cone-7 as the user corrected it). Depth is unbounded.
* **The alignment.** `align` is the selected round's fin, else the current
  round's fin, else 0, and the *rest* offset is re-anchored whenever the
  layout runs, so "at rest" always means "the current branch is at the
  front" (click a bead on a branch — nested trees included — to bring that
  branch round).
* **The driver, and why the wheel alone was not enough (v0.5.69).** The phase
  reads the scene's pan, and the scene only pans while its track is wider than
  the panel — `--cell` auto-fits, so a track of **17..45 columns fits
  exactly** and neither the wheel nor the drag could move anything at all
  (measured: `scrollLeft/clientWidth/scrollWidth` = `0/1200/1200` on a 4-fin
  session, `--rw-scroll` `"0"` for ever). `--rw-scroll` is therefore
  `scrollLeft + turn`, the *turn* being the part of a gesture the track could
  not take: the wheel accumulates its overshoot at either end, the drag sets
  it from the pointer's own wish. A scene that can pan behaves exactly as
  before, one value per frame; a scene that cannot, turns the cone where it
  stands. The turn resets with `--rw0` at every **layout point** (a new scene,
  a new selection, a resize, a session switch), so rest still means alignment,
  and a bare re-measure (`on:pointerdown`) leaves the cone alone.
* **The projection is taken *live* (v0.5.69).** The server's pop guard is the
  kernel's own — but the kernel only runs it between turns, when every call of
  the previous turn is answered, while the plugin projects on every request.
  A tool call whose result is not in the log yet is therefore **pending**, not
  stranded, while `is_running(session)`; without that rule a running session's
  tree dropped every marker and flattened to one line (measured: `0 fins`
  while the agent worked, `4 fins` a moment later). The rule is narrow: a pair
  the *mask* splits still strands, and a dead loop gets the kernel's exact
  behaviour.
* **The beads.** A branch's rounds ride its plane, each counter-rotated about
  its own dot so numbers stay upright; the plane itself is what tips into the
  screen. Clicking, hover tooltips and `elementFromPoint` all work on a turned
  branch. Two engine facts are load-bearing here and were measured the hard
  way: an element with `opacity < 1` becomes a **grouping element** and
  flattens its 3D children, so the park fade is a *value* (`--fade`) read by
  the leaves, never the `opacity` property on a container; and a bead must
  counter-rotate by its plane's **absolute** angle, which the render supplies
  (`--sroot`/`--sum`) because CSS cannot add an ancestor's variable to its own
  without a cycle. `hypot()`/`atan2()` are usable but not with container units
  inside them, so the cell and the slope cross into CSS as px (`--cell`, `--q`).
* **Degenerate cases.** No fork: no branch at all, the flat line the scene
  always was. One fork: a **swing** (`25°·sin(phase)`) instead of a cone that
  can park — one CSS line, no server field.
* **Less motion.** `prefers-reduced-motion: reduce` unfolds the same local
  geometry onto rows with the container's `scaleY(cos a − 0.12·sin a)` *and*
  pins the phase, so it is genuinely static; the 3D-off-but-still-sliding
  variant is not "reduced motion". A forced `.flat` class (the probes'
  affordance) shows the same projection while still following the scroll. A
  nested bead counter-scales by its whole chain's product (`--kup`/`--kown`
  beside the dynamic `--kroot`), so its dot stays round at any depth.

The old lane-vs-trunk hinge (`.rw3-hinge`) and the per-branch ribbon
(`.rw-fin`) with its elbow bars (`.rw3-elbow`) are **deleted** (D-cone-3);
the stylesheet mirror's extractor asserts their absence. Implementation
record, the three engine traps, the one honest limitation (a *parked*
branch's plane travels with its push, so its spine no longer lands exactly on
its parent's bead in projection) and the pixel measurements are **§13.6 of
`rewind-plugin-plan.md`**; the deep probe is `e2e/orbit_probe.py` (L1–L12,
**40 checks**, own fixtures) and the live-shape probe is section H of
`e2e/flow_style_b_probe.py`.

**v0.5.69 open item (still open).** The two live defects are covered by unit
tests and were confirmed against the live API, and the four probes have since
been re-run green (v0.5.70) — but the two *new* browser checks that would pin
them (a cone that turns on a scene too narrow to pan, and a `loop.pid` toggle
for the pending rule) were deferred at the user's request, because they were
reviewing that fix by hand.

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

## 4b. Rewind × compaction — what the plugin now shows (v0.5.61)

The kernel's projection rule lives in
`rushi/docs/rewind-fork-design.md` section 11: the compaction boundary
is the last `compaction_summary` **on the active path**, not the last
one in the log. The plugin was already correct — it appends one marker
and renders the tree — but it was silent about the three things a user
needs to see, and its tree had one disagreement with the projection.
This section is the summary; `docs/rewind-plugin-plan.md` section 11 is
the plan and the implementation record.

**R1 — the pre-check (server, pure, tested).** `rewind_verdict(events,
target_seq, mode) -> RewindVerdict { Ok | StrandsPair{missing} |
NotSettled{reason} }` re-evaluates the kernel's own decision: build the
log plus the candidate marker, take the boundary on the active path,
mask the active ranges, and run the kernel's
`context_strands_pairs` over the kept region. `POST
/api/sessions/{id}/rewind` refuses a pick the kernel would ignore with
`409` and the verdict in the body, and writes nothing — the log never
gains a marker that does nothing. `200` means it landed.

**R2 — the notices.** Before the write, the confirm dialog names what
the pick restores and, when it is refused, why. After the fact the
plugin area shows it: the tree carries `ignored` (every marker the
projection drops, outermost first — a port of the kernel's
`mask_active_path` pop loop) and `tail_ignored` (the log's last marker
*is* one of them). The post-hoc path is what covers markers the plugin
did not write — a TUI pick, the WS transport, a hand-edited log.

**R3 — the annotation.** Every node carries `restore`:
`Raw` (no boundary on the active path), `Framed{version, from_seq,
to_seq}` ("handoff v1 + raw 1..4"), or `Unresumable{missing}`. It is
the kernel's answer, computed server-side from the same rule, and it
is the user-visible form of decision D-D: a rewind to a round before
the last compaction shows `raw history`, and those rounds really are
rebuilt from their original events.

**The tree follows the drop.** A marker the kernel ignores does not
move the cursor: before v0.5.61 the scan let every marker move it, so
a log whose tail marker was ignored showed "you are here" one round
too far back and disagreed with the projection. The active path, the
round states (`active` / `abandoned`) and every annotation now come
from the markers that survive. All history stays in the file — the
abandoned rounds are still rendered.

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
| A very long chain (32+ rounds) | **Use the flow style.** The list style renders one DOM level per round and exhausts the wasm stack at roughly 32–80 rounds (the app survives, the tree does not paint, the console says `memory access out of bounds`); the flow style's scene draws every session measured here, up to 117 rounds. |
| A fork inside a branch (a nested fork) | Its rounds are attributed to their own branch and it gets its own fin, hinged on a column inside another fin — it renders, but no connector is drawn between the two. |

## 6. Verification

* `cargo test -p rushi-web` — the projection against the kernel fixtures,
  the flow layout, the ring's fin/hinge/round attribution, the per-round
  detail, and (v0.5.69) the two live-projection rules: a pending in-flight
  call never drops a marker, a mask-split pair always does
  (**71 tests green**).
* `trunk build` — the Leptos CSR bundle.
* `python3 e2e/orbit_probe.py` (v0.5.70) — **40 checks, own fixtures, own
  server**: the ray (`out = k+1` steps, one column each), the one slope and
  its auto-fit, the spine's span and its start on the parent's bead, the
  nested container inside its parent, the arc's queue and its parking order,
  the turn under scroll (every bead still on the law), the click-to-align of a
  nested tree, the flat projection (`scaleY` of the same rays, round dots at
  any depth) and `prefers-reduced-motion` pinning it.
* `python3 e2e/flow_style_b_probe.py [port]` — **66 checks** (A-I) on a live
  server, including section H's cone shape and section C's canaries that
  the ribbon (`.rw-fin`) and the elbow bars (`.rw3-elbow`) are gone.
* `python3 e2e/flow_check.py [port]` — the flow projection's invariants
  across **every session on the server** (8 sessions, 0 problems).
* `python3 e2e/rewind_probe.py [port]` — **140 browser + HTTP assertions**
  over four fixture sessions built by the probe
  (`/tmp/rw-e2e/sessions/rewindprobe`, forked; `rewindprobe2`, a second
  session; `rewindprobe3`, no round at all; `rewindprobe4`, the rewind ×
  compaction fixture of section 4b — a boundary inside an abandoned span,
  a pick that would strand a tool pair, and a marker the kernel ignores):
  the rebuilt expanded view (layout, rail, scroller), the rail switching
  sessions in-view, the recursive tree (abandoned / current / retracted /
  boundary / tooltips / legend / restore annotation), light + dark theme,
  the dialog copy and its English text, node-click ⇒
  `{"type":"rewind","target_seq":6,"mode":"on"}` appended and the marker
  moving live, **case C** (re-entering the abandoned branch — the log stays
  append-only), the card button (present, inert on the current tail, same
  dialog), the `#plugin-area` entry, the loop-running guard (with a live
  `loop.pid`: every node locked, no dialog, footer explains, card buttons
  disabled), the `409` refusals, the ignored markers and the tail notice.
* `python3 e2e/orbit_probe.py` — **34 browser assertions** (L1–L12) over
  fixtures the probe writes and serves itself (a sixty-round trunk with six
  forks off six early rounds plus a fork *inside* one of them, a one-fork
  session, a forkless one): the ring's structure, the geometry law
  recomputed in Python against the painted rects (rotateX **and** the
  perspective divide, to 3px), the bounded arc and its depth-ordered queue,
  the trunk unmoved and untransformed through a full turn, the phase riding
  on `--rw-scroll` alone, click-to-align, hit-testing a turned bead, the
  flat `−R·cos` projection, reduced motion genuinely static, the zero- and
  one-fork cases, the nested fin, and no wasm traps.
* `python3 e2e/flow_style_b_probe.py [port]` — **66 browser assertions**, the
  Style B surfaces end to end on the live sessions: the switch and its
  persistence; the 1:2 split and the x-only scroller; the scene (a dot per
  round, a run per main-line step and a run per forking branch, the lit live
  path, lane geometry, no dot overlap, the pitch floor and cap); the panel
  (D5 pre-select, the verbatim text, the button inert on the current round, a
  click that selects and opens **no** dialog); the button → the one dialog →
  cancel writes nothing; the 117-round session (pan, wheel, a horizontal
  delta left native, the flat list); persistence; the orbit section
  (`.rw-orbit` inside a real perspective, one fin per fork, the trunk on the
  axis and never turned, a turned fin as a real `matrix3d` with
  counter-rotated beads, the rest radius, the phase moving and re-anchoring,
  the flat projection, `prefers-reduced-motion` holding still); and the
  **paint** — accent pixels counted from real screenshots in the light *and*
  dark palettes (this is what caught the missing `--lane` on the line's runs).
* `python3 e2e/flow_check.py [port]` — the flow projection over every session
  the server knows (`main` chain, cols/lanes, each node's column and lane,
  the edge list, and since v0.5.68 the ring's own invariants: `orbit.fins`
  against the numbered fins, slots `0..n-1`, the trunk not numbered, `hinge_x`
  the parent's column, the trunk branch carrying exactly the main line, no
  round on two fins): 8 sessions, 0 problems on this host.
