# Rewind Plugin — Detailed Development Plan

Status: **implemented (2026-10-02)** — P1–P7 done, verified (`cargo test -p
rushi-web`, `trunk build`, `e2e/rewind_probe.py` 71/71). See §9 for the
step-by-step record. Supersedes the earlier `rewind-button-plan.md`.

All UI text is English (the webui is the English build).

## 1. Aligned decisions

| # | Decision | Resolution |
|---|----------|-----------|
| 1 | Where the tree lives | The sidebar **expanded view (`layout-full`) is rebuilt as a brand-new full-window "History" UI** — not a cap-lifted sidebar column, not an embedded block in the dispatch list. The current dispatch content is a placeholder and may be replaced/refactored. |
| 2 | Node granularity | **One node = one user message = one loop round.** The agent's work between two user messages folds into its node (summary + event count). |
| 3 | Rewind semantics | Click a node → **"reset to this point"**: that message stays the active tail, everything after it is abandoned, and the next context assembly ends there. Kernel vocabulary: `mode:"on"`, `target_seq` = that `user_message`'s 1-based log line. |
| 4 | Quick button | **Coexist**: a small `⟲` at the bottom-right of user-message cards in the transcript, plus the tree in the History view. Both open the same confirm dialog. |
| 5 | Loop running | **Rewind is forbidden while the session's loop runs.** Card button disabled; the History tree stays viewable but nodes are not clickable (no hover/affordance; a hint explains why). No `POST /stop` — wait for idle. |
| 6 | Dialog title | **"Rewind to this point?"** |

## 2. What already exists vs. what is new

| Piece | State |
|---|---|
| `rewind` event + `active_ranges` masking + claim settle semantics | ✅ kernel (`crates/rushi/src/rewind.rs`, `bin/claim`), tested |
| All branches preserved in one append-only `events.jsonl` | ✅ by design (nothing rewrites the log) |
| `POST /api/sessions/{id}/rewind` (write path, flock parity) | ✅ server (`main.rs` L240, `sessions.rs` L469) |
| Live re-render of rewind markers (WS tail) | ✅ (`main.rs` L540-590) |
| `rewind` divider card in the transcript | ✅ (`transcript.rs` L293/L647) |
| **Read-only history-tree projection** | ❌ new server module |
| **History view (new expanded UI) + tree rendering** | ❌ new client module |
| **Card `⟲` button + confirm dialog** | ❌ new |
| **Client rewind API calls** | ❌ new |

## 3. New expanded view ("History")

`layout-full` currently = the sidebar stretched to the window (`#app.layout-full
#sidebar { width: 100% }`, `#main` hidden) rendering dispatch groups. It becomes
a **dedicated full-window view**, mounted as a sibling of `#main` and hidden in
split/main:

```
#app
├─ #sidebar            (hidden when layout-full)
├─ #main               (hidden when layout-full)
├─ #history-view       ← NEW: shown only when layout-full
│  ├─ #hist-top        ◀ back to chat · "HISTORY — <session>" · loop lamp · theme
│  ├─ #hist-body (row)
│  │  ├─ #hist-rail    sessions grouped by project (compact cards, click = select)
│  │  └─ #hist-tree    the history tree (the scroller)
│  │     ├─ .rw-node × N   ● round i · summary · N events · time
│  │     │                 ├─ .rw-kids (recursive; abandoned = dimmed)
│  │     └─ .rw-current marker / .rw-boundary markers / legend
│  └─ #hist-foot       hint: "click a node to rewind · disabled while the loop runs"
└─ dialogs (incl. NEW RewindConfirmDialog)
```

- Selection: clicking a rail card sets `active_session` and loads that session's
  tree (`load_rewind_tree`). The active session is pre-selected.
- Node states: **active** (on the active path), **abandoned** (dimmed +
  strikethrough summary), **current** (the active tail: ring + "you are here"),
  **boundary** (a compaction marker: rewinds older than this land at the
  boundary).
- Click a node → `rewind_pending.set(Some(seq))` → `RewindConfirmDialog`.
  Disabled (press-and-hold nothing, `cursor: default`, tooltip
  "rewind is disabled while the loop is running") when
  `loop_running.get() || looping_sessions.contains(session)`.
- Closing (`◀ back to chat`) returns to `split`.
- Empty states: no session → "select a session"; session with no user message →
  "no rounds yet".

This is a brand-new interface: the existing dispatch chrome (30% plugin cap,
session-list in the sidebar) is not reused inside it; the rail reuses the
grouping helper (`dispatch_groups`) and, since v0.5.56, the dispatch view's
session card itself (`ui::session_card` + `ui::session_group_head`, see §9).

## 4. Tree data model (server projection)

New module `bin/rushi-web/src/rewind.rs` — a **pure function over the event
list**, so it is fixture-testable without I/O:

```rust
pub fn build(events: &[serde_json::Value]) -> RewindTree
```

(handler: `st.sessions.events(&id)` → `rewind::build` → JSON; a missing log
degrades to an empty tree, mirroring `essence.rs`).

Wire format (recursive; `children` nested):

```jsonc
{
  "session": "my-session",
  "total_events": 142,
  "roots": [ {
      "seq": 12, "round": 3, "ts": "2026-10-02T…",
      "summary": "first ~60 chars of the user message",
      "events": 7,                 // non-ext_status lines in this round
      "state": "active",           // "active" | "abandoned"
      "current": false,
      "retracted": false,          // a user_message_retract targeted this id
      "children": [ /* nested nodes */ ]
  } ],
  "rewinds": [ { "seq": 40, "target_seq": 12, "mode": "on" } ],
  "boundaries": [ { "seq": 61, "first_kept_seq": 61 } ],
  "current_seq": 58,               // the "you are here" node (null when no round)
  "pending_from": 58               // the next context assembly ends here
}
```

### 4.1 The builder (cursor + parent map)

Three passes over the events (1-based line number = `index + 1` =
the kernel's `seq`):

1. **Structural scan.** A `user_message` opens a round node (`seq`, `round`
   index, `ts`, `content`, `events` count until the next user message/rewind,
   `retracted` when a later `user_message_retract.target` matches its `id`).
   A `rewind` yields `(seq, target_seq, mode)` (reuse the same validation
   rules as `parse_rewind_event`: integer `target >= 1`, `mode ∈ {before,on}`,
   `target < seq`; malformed → ignored). A `compaction_summary` yields a
   boundary. Everything else is noise (but counts toward `events`).
2. **Cursor + parent map** (the fork structure):
   ```
   cursor = None                       // "root"
   for ev in order:
       user_message U →  parent[U] = cursor;  cursor = U
       rewind(S,T)    →  cursor = round_at(T)   // last user_message line ≤ T; None if none
   ```
   This is exactly the tree shape: a sequential round attaches to the previous
   round; a rewind re-attaches the *next* round to the target round, which is
   what makes a fork (the old next round stays as the other child).
3. **Active path & states.**
   ```
   active = set()
   n = cursor
   while n: active.insert(n); n = parent[n]
   ```
   `current = cursor`; `pending_from = current`; `settled` when the log's last
   structural event is a rewind marker with no round after it.

### 4.2 Correctness (proven against the kernel, unit-tested)

`active` (the cursor's parent chain) is **equivalent** to the kernel's
`active_ranges(total, rewinds)` ∩ rounds:

- The rounds on the active path are exactly the user-message lines inside
  `active_ranges`. The kernel's own fixtures become fixtures here:
  - `nested_forks_mask_the_abandoned_branch` (A→B→A′, rewind inside A′);
  - `reentering_a_branch_rebuilds_its_path` (A→B→A′, rewind back into B);
  - `deep_chains_follow_the_nested_targets` (3 nested forks);
  - `rewind_at_log_end_has_no_continuation` (settled at the marker);
  - `before_mode_excludes_the_target`.
- Each fixture is expressed as a tiny synthetic log; the test asserts both the
  tree's `active` set and (independently) a ported 20-line `active_ranges`
  reference over the same input — a divergence fails the build.

Porting (not importing) the reference keeps `rushi-web` free of a
`rushi-common` dependency (strict separation; the binary's build stays
independent).

## 5. Client module map

| File | Change |
|---|---|
| `web-leptos/src/model.rs` | `RewindNode` / `RewindTree` types (Deserialize); `AppState`: `rewind_tree: RwSignal<Option<RewindTree>>`, `rewind_pending: RwSignal<Option<u64>>`, `rewind_gen: RwSignal<u64>` (bumped on structure events) |
| `web-leptos/src/api.rs` | `load_rewind_tree(id) -> Result<RewindTree, String>`; `post_rewind(id, seq, mode) -> Result<(), String>` |
| `web-leptos/src/rewind.rs` | **NEW** plugin module: `HistoryView` (new expanded UI: top/rail/tree/foot), recursive `rw_node_view`, `RewindConfirmDialog`, `open_rewind(state, seq)` helper, the refetch effect |
| `web-leptos/src/lib.rs` | `mod rewind;`; mount `<rewind::HistoryView/>` (Show on `layout=="full"`) + `<rewind::RewindConfirmDialog/>` |
| `web-leptos/src/transcript.rs` | `⟲` button in `event_card_view` for `t == "user_message"` (it has the card index `key`); `target_seq = hist_oldest_line + key`; disabled per decision 5 |
| `web-leptos/src/ws.rs` | on an incoming `event` frame of type `user_message`/`rewind` → `rewind_gen += 1` |
| `web-leptos/style.css` | `#history-view` + `#hist-*` + `.rw-*`; hide `#sidebar`/`#main` in `layout-full`; card-button CSS; register the tree scroller in **all 5** scrollbar lists (plugin rule 4) |
| `bin/rushi-web/src/rewind.rs` | **NEW**: `RewindTree` + `build()` + unit tests |
| `bin/rushi-web/src/main.rs` | `mod rewind;`; extend the route to `get(get_rewind_tree).post(post_rewind)` + handler |

No other `plugins.rs` change beyond the C1 entry (N = 3):
`#plugin-area` shows **goal · essence · rewind** so the loaded plugin set is
always visible. The rewind plugin-area panel is a compact summary (rounds,
branches, current round) + an "open History view" button; the full tree is the
expanded view.

## 6. Step plan

**P1 — server projection (`rewind.rs`)**
`RewindTree`/`RewindNode` (serde), `build(&[Value])`, unit tests with the five
kernel fixtures + a synthetic multi-fork session.
*Accept:* `cargo test -p rushi-web` green; on a probe session,
`GET /api/sessions/<id>/rewind` returns the tree and `active` matches
`active_ranges` for every fixture.

**P2 — client plumbing**
`model.rs` types + signals; `api.rs` `load_rewind_tree` / `post_rewind`;
`ws.rs` `rewind_gen` bump; refetch effect (mount, `active_session` change,
`rewind_gen`).
*Accept:* `trunk build` clean; a console probe logs the tree for the active
session; session switch reloads it.

**P3 — History view: layout + rail**
New full-window `#history-view` (top bar, rail, tree host, foot); CSS to hide
`#sidebar`/`#main` in `layout-full`, show `#history-view`; rail = compact
grouped session cards; `◀ back` returns to `split`.
*Accept:* the expand chevron (`»`) opens the new view; rail selects sessions;
the transcript is fully replaced; light + dark theme clean.

**P4 — tree rendering**
Recursive `rw_node_view` (precedent: the file tree's `tree_node`/`tree_level`,
`ui.rs` L2942-3034): connector lines via `::before`, dimmed abandoned
branches, `current` ring + "you are here", boundary markers, legend, node
tooltips (round, time, event count, state).
*Accept:* a fixture session with nested forks renders trunk + nested dimmed
branches; the current node is correct; scrolling works inside the view.

**P5 — confirm dialog + rewind action**
`RewindConfirmDialog` (modeled on `DeleteConfirmDialog`): title
"Rewind to this point?"; body per §8/C3; Cancel / Rewind. Confirm (enabled only
when idle) → `post_rewind(id, seq, "on")` → close → refetch; errors keep the
dialog open with a red line.
*Accept:* click node → dialog → confirm → the appended `rewind` line targets
exactly that node's seq with `"mode":"on"`; the tree refetch shows the new fork
and the new `current`.

**P6 — card quick button**
`⟲` bottom-right of `.ev-user` cards; `target_seq = hist_oldest_line + key`;
disabled while the loop runs (tooltip) and on the current tail; same dialog.
*Accept:* click → dialog → confirm → same appended line; the idle card's button
is inert; the running card's button is disabled.

**P7 — probes & docs**
CDP probe (`e2e/` pattern) on a fixture session: assert tree shape, node click,
dialog copy, appended rewind line, `current` move, re-entry into an abandoned
branch (e2e case C), and the disabled state while looping. `docs/rewind-plugin.md`
(design + how the projection maps to kernel semantics), README changelog
(next v0.5.x), commit.
*Accept:* probe green; doc committed.

Effort: P1 1d · P2 0.5d · P3 1d · P4 1d · P5 0.5d · P6 0.5d · P7 1d
→ **~5.5 days.**

## 7. Edge cases & accepted limits

- **Compaction floor:** a node older than a boundary's `first_kept_seq`
  is shown, but rewinding there degrades to the boundary (kernel rule I4);
  the boundary marker + tooltip state this.
- **In-flight loop:** decision 5 removes the race by refusing the action.
- **Settle at a marker:** a rewind with nothing after it → `current` = the
  target, `pending_from` = it; the UI reads "conversation resumes here".
- **Tool-result targets** (TUI-written `tui_pick` markers targeting a
  `tool_result`): the node resolves to the round containing that line; a
  display simplification, never produced by this plugin (it always targets a
  user message's own line).
- **Retracted messages** (`user_message_retract`): non-structural; the node
  carries a `retracted` badge, no tree change. Optional v1.1 polish.
- **Window vs. full log:** the tree uses the server's full-log projection (the
  client's window is trimmed by design — `win.rs`); the card button uses
  `hist_oldest_line + index` for rendered cards only.
- **Multi-client:** the WS tail broadcasts the marker; every client bumps
  `rewind_gen` and refetches.
- **Performance:** one file read + parse per structure-changing event; a
  multi-MB log is a one-off parse (incremental building is a v2 candidate).

## 8. Confirm-points — RESOLVED (user, 2026-10-02)

- **C1 — YES, add a `#plugin-area` entry.** Every loaded plugin must appear in
  the shared plugin module so the user can see what is loaded. → `plugins.rs`
  gains `PluginDef { id: "rewind", label: "rewind" }` (N = 3) and
  `ui.rs::plugin_view` gains a `rewind_plugin_view` arm: a compact panel that
  summarizes the active session's tree (round count, branch count, current
  round) and offers an "open History view" button. The full tree still lives
  in the rebuilt expanded view (`#history-view`).
- **C2 — YES, keep the session rail** in the History view (stay full-screen
  while switching sessions).
- **C3 — dialog:** title **"Rewind to this point?"**, buttons
  **Cancel** / **Rewind**. Body: the C3 copy below (no "irreversible" claim):
  *"The active conversation resumes from here. Everything after it moves to an
  abandoned branch — it stays in the history tree, and you can rewind back to
  it later."*

## 9. Implementation record (2026-10-02)

| Step | Delivered | Verified by |
|---|---|---|
| P1 | `bin/rushi-web/src/rewind.rs` (pure `build()`, recursive nodes, ported `active_ranges` reference); `GET /api/sessions/{id}/rewind` next to the pre-existing POST | `cargo test -p rushi-web` → 42 passed; a fixture probe showed the tree for a forked log (fork at seq 8 → abandoned C(6) + active D(9)) |
| P2 | `model.rs` `RewindNode`/`RewindMarker`/`RewindBoundary`/`RewindTree` + `rewind_tree`/`rewind_pending`/`rewind_gen`; `api.rs` `load_rewind_tree`/`post_rewind`; `ws.rs` per-frame `rewind_gen` bump (structure types only); `rewind.rs::register_tree_effect` (one fetch for both surfaces) | `trunk build` |
| P3 | `#history-view` full-window UI (top bar with back/loop lamp/theme, `#hist-rail`, `#hist-tree`, `#hist-foot`); `layout-full` now hides `#sidebar`/`#main` instead of stretching the sidebar | probe: layout classes, rail selection (in-view switch + tree reload), scroller, both themes |
| P4 | recursive `node_view` (round/dot/summary/events/time, dimmed abandoned + strikethrough, `here` ring, retracted badge, `.rw-kids` guides, legend, boundary footnotes, full tooltips) | probe: 4 nodes, 1 abandoned, current, badges, meta, tooltips |
| P5 | `rewind_confirm_dialog` (title "Rewind to this point?", the C3 body, Cancel/Rewind, Rewind disabled when locked) → `post_rewind(seq, "on")` + `rewind_gen` bump + refetch | probe: dialog copy (English), cancel = no write, confirm ⇒ appended `{"type":"rewind","target_seq":6,"mode":"on"}`, live `current` move, case C re-entry, 14-line append-only log |
| P6 | `⟲` on every `.ev-user` card (`transcript.rs` → `quick_rewind_button`, `target_seq = hist_oldest_line + index`), inert on the current tail, same dialog | probe: 4 buttons, `[F,F,F,T]` disabled, tooltips, dialog target |
| P7 | `docs/rewind-plugin.md`, README section + API row, `plugin-authoring-rules.md`/`plugin-area.md` registry notes, `e2e/rewind_probe.py` | probe 71/71 PASS; `e2e/layout_probe.py` still PASSes (the capsule-scrollbar lists) |
| R1–R3 | the rail = the dispatch cards, grouped by working path, loop toggle included (§9.1) | probe 88/88 PASS |
| L1–L3 | the group label: full-path tooltip on the basename + a display-only alias per working path (§9.2) | probe 107/107 PASS |

### 9.1 v0.5.56b — the rail becomes the dispatch view's cards (user request)

Request: *the expanded view's session cards should be sorted/grouped by
working path, and the previous placeholder expanded view's card design should
come back — that one was better, it has buttons to control the loop's
start/stop.*

| Step | Change |
|---|---|
| R1 | `ui.rs`: `dispatch_card` split into `pub(crate) fn session_card(state, s, stay)` + `pub(crate) fn session_group_head(key, count)`; the dispatch view (`dispatch_card`) is a thin wrapper with `stay = false`, so the two surfaces cannot drift |
| R1 | `rewind.rs::hist_rail` renders `session_group_head` + `session_card(..., stay = true)` per `dispatch_groups` bucket — one group per working path (`cwd` marker), the "(no project)" bucket for markerless sessions, the full path in the head's title, the card count beside it |
| R1 | `style.css`: the rail is the 250px dispatch column again; the `.rw-sess*` / `.rw-group*` placeholder rules are deleted; rail-scoped compaction of `.dispatch-card` (`.dc-name/.dc-time/.qa/.dc-actions`) |
| R2 | the card's loop toggle (▶ start / ■ stop over REST) is live in the rail, and the `…` rename/delete menu works there too (`#sess-menu` is `position: fixed` and mounted outside `#sidebar`, so it was never hidden by `layout-full`) |
| R3 | `e2e/rewind_probe.py`: +17 checks → 88 PASS. The three fixtures get `.cwd` markers (`alpha-project` / `beta-project` / none) so the path grouping is asserted for real, plus the toggle labels/classes/tooltips, and a **real ■ stop click**: the fake `loop.pid` is now started with `setsid` (so the server's `kill(-pid)` group signal lands) and is **reaped before** the server's liveness probe reads it (a zombie keeps `kill(pid, 0)` true — the fixture trap that made the first version of the check fail); the check then reads `/api/loops` (the session left `running`) and reloads to see ▶ start + a clickable tree |

Verified: `cargo test -p rushi-web` 42 passed; `trunk build` clean;
`e2e/rewind_probe.py` 88/88 PASS; `e2e/layout_probe.py` on the live server
still PASS (no shell regression from the rail CSS).

### 9.2 v0.5.57 — the group label: tooltip + display-only rename (user request)

Request: *keep basename-only labels, but hovering the basename must show the
full working path; also let the user customize the basename without changing
the real working path.*

| Step | Change |
|---|---|
| L1 | `ui.rs::session_group_head` renders the label as its own `<span class="dispatch-group-name" title=<full path>>` — the tooltip moved off the whole head row onto the basename (`…/rushi` vs `…/rushi/rushi` are distinguished by hovering) |
| L2 | a **display-only alias per working path**: `ui.rs` gains `read_project_labels` / `persist_project_labels` (localStorage `rushi-project-labels`, same pattern as `rushi-sort-mode` / `rushi-custom-order`), `group_label(state, key)` (alias else basename) and `set_group_label`; `AppState` gains `project_labels` + `group_edit`; the head shows a hover-revealed `✎` that swaps the label for an input (Enter/blur commit, Escape cancel, empty = reset to the basename) |
| L2 | the real path is never written: no server call, nothing in the session dir. The probe asserts the server's `cwd`, the event log and the `loop.last` marker are all unchanged by a rename |
| L2 | M7's uppercase transform is kept for a basename, dropped for a custom alias (`.dispatch-group-name.custom`) so a rename reads as typed |
| L3 | `e2e/rewind_probe.py`: +19 checks → 107 PASS (label/tooltip per group, pencil opens/prefills, Escape, Enter commit, as-typed rendering, real path + tooltip intact, persistence across a reload, clear → basename, localStorage dropped) |

Kept as-is (user decisions this round): the rail card click stays `stay = true`
(in-view switch, C2), and the loop toggle stays live while the loop runs —
otherwise a stop button would be pointless.

Extra (beyond the plan text, decided during implementation):

- The dialog is **one component** shared by the tree, the panel rows and the
  card button (the plan described the card button as "the same dialog").
- The History top bar carries the **loop lamp** (§3's sketch) as the visible
  reason the tree is read-only.
- The panel's rows are the **active path** (one click per round) rather than a
  static summary only.
- `#history-view`'s tree scroller is `#hist-tree` (plan §3 naming) and is
  registered in all five capsule-scrollbar rule lists (plugin rule 4).

---

## 10. Style B — the 3D "flow" tree (plan, in progress)

**Request (user, 2026-10-03, Chinese):** keep the expanded view's current form
and **add a second style** to it. In the new style:

* the session's **main line is a horizontal `. - . - . - .` chain of nodes**;
  a rewind **forks** off it (up or down). Ideally the branches turn in 3D as
  the wheel scrolls ("旋流光" — a rotating light sweep), with the **longest
  chain as the straight main line**;
* the **node the session is currently at is highlighted**;
* **no per-node text** in the graph (unlike Style A);
* the right-hand main area splits **top / bottom ≈ 1 : 2** — **bottom** = the
  clickable graph, **top** = the details of the clicked node (the user's
  message for that round, …) **and the Rewind button**;
* **clicking a graph node only selects it** — it must not rewind. Rewind is
  triggered from the top panel only.

Style A (the recursive round list, §9) stays byte-for-byte; this is an
additional view, switched from the History top bar.

### 10.1 Research findings that shape the design

1. **The client crate cannot be unit-tested.** `cargo test -p rushi-web-ui`
   fails to build for the host (233 errors — every `web_sys`/`wasm-bindgen`
   use is `cfg(target_arch = "wasm32")`; only `markdown.rs`'s 7 tests ever
   compiled). So any **logic** worth testing (which chain is the main line,
   each node's lane/side, the edge list) must live in the **server
   projection**, where `cargo test -p rushi-web` covers it in 0.3 s. This
   matches the existing rule that the tree is computed server-side.
2. **`animation-timeline: scroll()` is not safe to depend on.** Chrome/Edge
   115+ and (per published notes) Firefox 133+ have it, but Safari's status
   is contested in the sources (WebKit's own tracking issue is still open for
   stable shipping). We drive the scene from a **scroll proxy + one CSS
   custom property** (`--rw-t`, written by a single `on:scroll` handler):
   universally supported, no per-frame JS work beyond one var write, and it
   keeps native wheel/trackpad momentum. `animation-timeline: scroll()` stays
   an optional `@supports` fast path later.
3. **`preserve-3d` pitfalls (must be designed around, not discovered).** An
   ancestor with `overflow` ≠ `visible`, `filter`, `opacity` < 1,
   `clip-path`, `mask` or `contain: paint` **flattens** its subtree — the 3D
   goes away silently. Therefore: the scroller (`#rw-flow-scroll`,
   `overflow-y: auto`) may **not** be the perspective element; the
   `perspective` element is a `position: sticky` viewport *inside* it, and
   clipping is done by the outer panel. Node labels **counter-rotate**
   (`rotateX(-t)`) so text stays upright/readable while the line turns.
4. **Design language constraints** (`docs/relief-shadow-pipeline.md`): relief
   is **directional** — no omni-directional halos. So the "light" is a
   *directional* gradient sweep along the line, not a glow blob; the current
   node is marked with the existing accent ring + `--shadow`, not a halo.
   `prefers-reduced-motion: reduce` must yield a static, flat, readable view
   (the repo honours it in 9 places).
5. Long sessions: the main line's logical width is `rounds × STEP`, which can
   be tens of thousands of px. v1 **auto-fits** the scene to the panel width
   with a **minimum scale floor** and **drag-to-pan**, so `--rw-t` (wheel) is
   never fighting a horizontal scrollbar.

   → **the width estimate was wrong (§10.8, corrections).** The cell caps at
   64 px, so 117 rounds is ~7 500 px, not tens of thousands — and the scene
   *is* a horizontal scroller (`overflow-x: auto`, scrollbar hidden). The
   wheel proxy takes the vertical delta and pans along the line, so there is
   no vertical scroll for it to fight; a horizontal delta stays native.

### 10.2 Layout algorithm (server, pure + tested)

* **Main line** = the longest root→leaf chain over the *whole* tree (count of
  nodes); ties break to the chain containing the current node, then to the
  leftmost. The user asked for exactly this ("the longest chain is the
  straight main line"), and it keeps the picture stable across a rewind: the
  abandoned branch often *is* the longest, and the fresh short branch forks
  off it.
* Every node gets `x` = its distance along its own chain (0,1,2,…) and
  `lane` = 0 on the main line. Off-main children are assigned alternating
  lanes by their index among *that fork's* off-main siblings: 1st → `-1`,
  2nd → `+1`, 3rd → `-2`, 4th → `+2`, … (`-` = up, `+` = down). A lane is
  never shared by two chains that would overlap on the same `x` (checked in a
  test); the client multiplies `(x, lane)` by its own pixel constants, so the
  server owns *logical* geometry only.
* Uniform `STEP` along x is possible because **nodes carry no text** — a node
  is a dot; its label lives in the top panel. (This is what makes the "rotating
  `. - . - .`" picture tractable at all.)
* The projection adds `flow: { nodes[{seq,round,x,lane,main,side,state,
  current,retracted,events,ts,summary}], edges[{from,to}], cols, lanes,
  main_len }` to the **existing** `GET /api/sessions/{id}/rewind` response —
  one fetch, one cache, one invalidation path (the WS-driven refetch already
  exists). Style A ignores the extra key.

### 10.3 Step plan

**Landed — as-built record in §10.8 (B1–B9, v0.5.63–v0.5.66). The table
below is the plan as written; §10.8 lists every place the build diverged
from it and why.**

| step | what | files | proof |
|---|---|---|---|
| **B1** | flow layout projection: `main_chain`, `layout` (x/lane/side), `edges`, `Flow` types | `bin/rushi-web/src/rewind.rs` | unit tests: longest chain wins; tie→current; alternating lanes; no lane collision; single node; empty; a rewind fork puts the abandoned branch off-line; `cols/lanes/main_len` |
| **B2** | node detail route `GET /api/sessions/{id}/rewind/node/{seq}` → `{seq,round,ts,text,events,state,current,retracted}` (the full `user_message` content, not the 60-char summary) | same | unit tests: exact text, unknown seq → 404, retracted flag; route registered next to the tree route |
| **B3** | client data: `RewindFlow`/`FlowNode`/`FlowEdge`/`RewindDetail` types, `api::load_rewind_detail`, state `rw_view` (localStorage `rushi-rw-view`, plugin-owned), `rw_selected`, `rw_detail`; clear `rw_selected` on session change | `web-leptos/src/model.rs`, `api.rs`, `rewind.rs` | `cargo check` (wasm) clean; `trunk build` |
| **B4** | the style switch in `#hist-top` (`[ tree \| flow ]`, persisted) + `hist_tree` dispatches to Style A (unchanged) or `flow_view` | `web-leptos/src/rewind.rs` | probe: switch persists across reload; Style A's existing 110 checks still pass |
| **B5** | the 1:2 split: `#rw-split` → `#rw-detail` (top, flex 1) + `#rw-flow` (bottom, flex 2); detail panel = round/time/events/state header + the user's message (pre-wrap, own scroller) + `⟲ Rewind` button + empty state | `rewind.rs`, `style.css` | probe: panel appears; selecting a node fills it; button disabled states |
| **B6** | the scene: SVG connector plane (`. - . - .` line + forked edges + dots) with one absolutely-positioned node button per round (`data-x/y/lane/main/seq`), current-node highlight, selected-node ring, `title` tooltip; auto-fit scale (floor) + drag pan | `rewind.rs`, `style.css` | probe: geometry assertions (main-line nodes share y; a fork's node has Δy≠0), click selects and **opens no dialog** |
| **B7** | the 3D + light sweep: `#rw-flow-scroll` (tall spacer) → sticky `#rw-flow-view` (`perspective`) → `.rw3-scene` (`preserve-3d`, `rotateX(var(--rw-t))`), counter-rotated labels, directional sheen travelling along the main line with `--rw-t`; `prefers-reduced-motion` → flat/static | `rewind.rs`, `style.css` | probe: wheel changes `--rw-t`; rotation is monotonic; reduced-motion path renders; no flattening (computed `transform-style` is `preserve-3d`) |
| **B8** | Rewind-from-the-panel: the top button calls the existing `request_rewind` → the same confirm dialog (§9.3 copy; the dialog is the one confirm point) | `rewind.rs` | probe: button → dialog → (fixture) post; guard while the loop runs |
| **B9** | docs + mirror: `docs/rewind-plugin.md` (the second style), this record, `rushi-rewind` sync + `install/TOUCHPOINTS.md` §3.9 + probe re-run from both homes | docs, `rushi-rewind/` | `cargo test -p rushi-web`, `trunk build`, probe from both homes |

Verification per step, as always: `cargo test -p rushi-web` (server),
wasm `cargo check` + `trunk build` (client), `e2e/rewind_probe.py` (CDP),
`e2e/layout_probe.py` on :8480 for the shell.

### 10.4 Decisions to align (recommended default in bold)

> **All nine were confirmed by the user on 2026-10-03 before B1 started;
> each entry below carries what was built.** D1 ✅, D2 ✅ (list stays the
> default), D3 ✅ (the split is flow-only), D4 ✅ (auto-fit floor + drag,
> no zoom UI), D5 ✅, D6 ✅ (native `title` only), D7 ✅ (the scroll proxy
> takes over *only* the vertical wheel delta; a horizontal delta stays the
> browser's), D8 ✅ **via B2's route** (see the note at D8), D9 ✅ (native
> `view(x)` where supported, proxy elsewhere, static under
> `prefers-reduced-motion`).

* **D1 main line**: longest chain over the whole tree, ties → the chain that
  holds the current node (then leftmost). **Confirm**, because an abandoned
  branch can be the straight line and the current node then sits on a fork.
* **D2 default style**: **`tree` (today's view)** — the new style is opt-in
  via the switch; or make `flow` the default once it settles.
* **D3 scope of the 1:2 split**: **Style B only**. Style A keeps its current
  full-height list (ask whether Style A should later gain the same panel).
* **D4 long sessions**: **auto-fit with a scale floor + drag-to-pan**; a
  zoom slider / mini-map is v2.
* **D5 initial selection**: **the current node** is pre-selected when the
  flow view opens, so the top panel is never empty ("you are here" + its
  message, and the Rewind button correctly disabled there).
* **D6 node hover info**: dots only, with a **native `title` tooltip**
  (round · summary) — no text in the graph; the panel is the text surface.
* **D7 wheel semantics**: wheel = rotate (via the scroll proxy); horizontal
  panning is drag (and shift+wheel), never a horizontal scrollbar.
* **D8 (added 2026-10-03) where the full message text comes from**: either
  **B2's per-node route** (`GET .../rewind/node/{seq}`, lazy + bounded, one
  more request and a loading state per selection) or the **tree response
  carries it** (simpler: no route, no `rw_detail` state, no loading flicker;
  measured cost 4–47 KB per session, §10.6 finding 4 — a cap, e.g. 4 000
  chars, keeps it bounded). **Recommended: carry it in the tree** and drop
  B2's route.
  → **DECIDED (user, 2026-10-03): the per-node route (B2).** The text is
  served per round, verbatim and never summarized, fetched lazily on
  selection; the recommendation above is *not* what was built (§10.8).
* **D9 (added 2026-10-03) how the 3D rotation is driven**: **CSS
  scroll-driven animation (`animation-timeline: view(x)`) where supported,
  the §10.7 scroll-proxy fallback elsewhere** (Firefox), and always a flat,
  static view under `prefers-reduced-motion`. Chrome 115+/Safari 26+ get the
  compositor-driven version for free (§10.6 finding 1); the proxy keeps one
  code path alive for Firefox and doubles as the reduced-motion path.

### 10.5 Non-goals (v1)

No tree editing, no in-graph search/jump box, no manual zoom UI, no touch/
pinch gestures (the webui is a desktop surface), no change to the kernel or
to Style A's markup, and no new server-side truth: the flow layout is a
*projection* of the same tree the kernel's `active_ranges` already proves.

### 10.6 Refresh (2026-10-03 — the research re-run)

> **Superseded by §10.8** (kept as the record of the research round):
> B1–B9 all landed in v0.5.63–v0.5.66, and D8 was decided the other way.
> Findings 1–3 stand; finding 4's measurement stands; finding 5 (in §10.1)
> was wrong about the scale — see §10.8 "corrections".

**Status: plan only, no code yet.** §10 was written and committed as
`b8b1d48` (*v0.5.60: plan — the rewind flow view (Style B)*) from the same
request (this session's round 13, log seq 6670); the session then moved on to
the rewind × compaction defect and to the loop-liveness fix. Verified while
re-reading the tree today: **no Style-B code exists** — `Flow` / `main_chain`
/ `lane` are absent from `bin/rushi-web/src/rewind.rs`, `rw_view` /
`flow_view` / `rw_selected` from `web-leptos/src/`, and `e2e/rewind_probe.py`
mentions only `overflowY`. B1–B9 are all still open.

Four findings that change or firm up §10:

1. **Browser support — resolved (was "contested").** MDN browser-compat-data
   (`css/properties/animation-timeline`, read 2026-10-03): `scroll()` and
   `view()` are **Chrome/Edge 115+**, **Safari 26+**, **Firefox `preview`**
   (behind a flag). §10.1.2's "Safari's status is contested, WebKit's own
   tracking issue is still open" is stale — Safari 26 shipped it. So the
   compositor-driven path is available on every browser the probes drive plus
   Safari, and only Firefox needs the proxy → **D9**.
2. **The `preserve-3d` flattening rule is confirmed verbatim** (MDN
   `transform-style`): *"grouping property values … force the element to have
   a used value of `transform-style: flat`, even when `preserve-3d` is
   specified"* — and `overflow` ≠ `visible` is one of them. §10.1.3's DOM
   constraint (the scroller must **not** be the perspective element; the
   perspective lives on a `position: sticky` viewport inside it) is therefore
   load-bearing design, not a micro-optimisation. Keep it.
3. **Real tree shapes — what the new style will actually be drawn from**
   (live server on :8480 + the probe fixtures):

   | tree | rounds | rewinds | shape |
   |---|---|---|---|
   | `Time inject` | 11 | 1 | spine **8** nodes; **one 3-node abandoned branch** forking off round 5 (seq 2114) — the only real fork available in the wild |
   | `rewindprobe` (probe fixture) | 4 | 1 | spine `r1,r2,r4` (3); abandoned `r3` forking off `r2` — the deterministic DOM/geometry fixture |
   | **`rewind` (this session)** | 30 | 4 | a **straight 30-node line**: all four markers are `ignored` by the P4 pair-stranding guard (`tree.ignored` = 4), so no fork to look at here |
   | `Webui` / `alpha` | 117 / 82 | 0 | straight lines, the long-session stress cases (17 k / 26 k log lines) |

   Practical consequence: **check the visuals on `Time inject` and the probe
   fixtures**, not on this session — and add a probe fixture whose spine is
   *shorter* than an abandoned branch, because that is the case D1 is about.
4. **D8 measured**: the full user-message text of a whole session is **4 KB
   (27 rounds) / 47 KB (117 rounds)** — avg 85–403 B/round, longest single
   message 1.2 KB — against `events.jsonl` files of 16–29 MB. The tree
   response is refetched only on a structural WS frame, so carrying the text
   in the tree costs ~0.05 % of the transcript the client already loads in
   full for the chat view. That is what makes D8's "carry it in the tree"
   recommendation cheap — **but the user chose the per-node route instead**,
   preferring a bounded payload per interaction over a fatter tree.

### 10.7 Why the wheel drives a CSS variable (not `scroll-behavior`, not JS transforms)

Kept from the original design, restated because D9 makes the native path
primary:

* The scene's rotation is a **pure function of the scroll offset** of one
  scroll container, so it can be expressed as a CSS animation on a scroll
  timeline (`view(x)` / `scroll(x)`) — the compositor interpolates it, no
  per-frame main-thread work, and native momentum scrolling keeps working.
* The fallback for browsers without the timeline (Firefox today) is **one
  `on:scroll` handler writing one custom property** (`--rw-t`) on the scene
  element: a single style-invalidation, no layout read, no per-node work.
  It is also the `prefers-reduced-motion` path (write `--rw-t = 0` once, so
  the scene is flat and static).
* Nothing reads layout inside the handler (no `getBoundingClientRect`, no
  scroll-height math per frame) — that is the failure mode that makes
  scroll-driven scenes janky, and the one the `full_fetch_probe.py` timeline
  would catch.

### 10.8 As built (2026-10-03, v0.5.63 – v0.5.66)

**Status: B1–B9 landed and verified.** The two styles are one tree read two
ways: Style A unchanged, Style B a projection of the same response (§10.2's
rule — one fetch, one cache, one invalidation path).

| step | version | commit | what shipped |
|---|---|---|---|
| **B1** | v0.5.63 | `87d63b1` | server flow projection in `bin/rushi-web/src/rewind.rs`: `Flow { nodes, edges, branches, cols, lanes, main_len }`, `fn flow`, helpers `layout_x`, `primary_children`, `place`, `pick_lane`; `RewindTree.flow`. **8 unit tests** |
| **B2** | v0.5.64 | `830781d` | `rewind::node_detail(&events, seq) -> Option<NodeDetail>`, route `GET /api/sessions/{id}/rewind/node/{seq}` (200/404), handler `get_rewind_node`. **3 tests** (65 total) |
| **B3–B7** | v0.5.65 | `b4ab0b7` | the client types/state/API (`RewindFlow`, `FlowNode`, `FlowEdge`, `FlowBranch`, `RewindDetail`; `rw_view`/`rw_selected`/`rw_detail`), the `[ list \| flow ]` switch, `#rw-split` 1:2, the detail panel, the scene, the 3D ribbons + the light |
| — | v0.5.65 | `38ae6a2` | the serde recursion fix the style forced into the open (below) |
| **B9** | v0.5.66 | *this commit* | this record, `docs/rewind-plugin.md`, the mirror + `install/TOUCHPOINTS.md` §3.9 |

**Every D1–D9 decision is implemented as confirmed** (§10.4). Where the
*build* differs from the §10 spec, it is listed here rather than quietly
diverging:

1. **The scroller is x-only.** §10.1/§10.6 assumed a tall spacer with
   vertical scroll and a sticky viewport inside it. Built instead:
   `#rw-flow-scroll` scrolls **only x** (`overflow-x: auto`,
   `overflow-y: hidden`, no scrollbar) and the lane pitch is a **percentage
   of the scene's own height** (`--rowp: calc(50% / (var(--lanes) + 0.5))`),
   so every lane always fits exactly one screen and no vertical scroll can
   exist for the wheel to fight. The load-bearing §10.1.3 rule is respected:
   the scroller is **not** the perspective element — `#rw-flow-track` (a child
   of it) is, with `.rw3-grid { transform-style: preserve-3d }`.
2. **Nothing rotates but the ribbons.** §10.1.3 wanted counter-rotated
   labels; the dots and runs are never transformed at all, so labels stay
   upright for free and there is nothing to counter-rotate (asserted: a dot's
   computed transform is a 2D `matrix(...)`).
3. **The light is a sticky band, not a travelling sheen.** One `.rw3-sheen`
   pinned to the panel (`position: sticky; left: 0`, `pointer-events: none`)
   — the scene slides under one fixed light. Same read, no per-node work.
4. **The driver is `--rw-scroll`, and the turn is per ribbon**:
   `--turn: clamp(-1, (var(--bx) - var(--rw-scroll) - var(--halfpw)) / var(--denom), 1)`,
   `transform: rotateX(calc(var(--turn) * 62deg))`. `--bx` (the ribbon's own
   centre), `--halfpw` and `--denom` are written at **layout** time (mount /
   tree change / selection / `resize`); a scroll frame writes **exactly one**
   value, so nothing reads layout while scrolling. `on:wheel` and the drag
   write it, and so does `on:scroll` — a scrollbar drag or a programmatic
   `scrollLeft` must not leave it stale (the probe's H7 found that).
5. **`animation-timeline: view(x)` is the primary path** (§10.6 finding 1)
   behind `@supports`, with the proxy elsewhere and
   `@media (prefers-reduced-motion: reduce) { transform: none }`.

#### Corrections to §10 (kept honest)

* **§10.1 finding 5 / finding "scale" — wrong.** "the main line's logical
  width … can be tens of thousands of px … so `--rw-t` is never fighting a
  horizontal scrollbar": the cell caps at 64 px, so 117 rounds is ~7 500 px,
  and the scene **is** a horizontal scroller. There is no vertical scroll
  (see 1 above), so the wheel proxy takes over **only the vertical delta**
  (`|dy| ≥ 0.5`); a horizontal delta — or shift+wheel — stays the browser's
  native x-scroll. One gesture, one meaning, never doubled (D7 ✅).
* **§10.1 finding 2** ("`animation-timeline: scroll()` is not safe to
  depend on") is superseded by §10.6 finding 1 (Safari 26 shipped it); the
  build uses `view(x)` as an enhancement on top of the proxy regardless.
* **§10.4 D8** — the user confirmed **B2's per-node route** over the plan's
  "carry it in the tree" recommendation: the text is fetched per round,
  verbatim, never summarized.

#### Two defects found while verifying, both fixed here

1. **serde_json's 128-level recursion limit hid half the sessions.** The
   tree nests one `children` array per round, so a chain past ~63 rounds made
   the client reject the **whole** response; `register_tree_effect` mapped the
   error to `Err(_)` and History sat on "loading…" forever. Fixed with
   `serde_json` `unbounded_depth` + `api::parse_deep` on the two routes that
   nest (tree, round detail), and the failure is now reported on the console
   (`rewind tree ({session}): {error}`). Measured live before the fix:
   `Webui` (117), `alpha` (82), `issue` (30) and `theme` (16) — 4 of 8
   sessions — never rendered.
2. **The main-line runs lost their `top`.** `.rw3-seg` was emitted without
   `--lane`, so `calc(50% + var(--lane) * var(--rowp))` was invalid, `top`
   fell back to `auto`, and every run took its *static flow* position — a
   cascade of stray bright bars down the panel. The DOM assertions never
   noticed (the elements existed; the dots were right); it was found by
   **photographing the panel** and counting accent pixels. `--lane: 0` is
   emitted now (with a `var(--lane, 0)` fallback in the CSS), a nested fork
   no longer emits a zero-height hinge, and section C/I of the probe check
   the connector rows and the paint in both palettes.

#### Style A's long-chain limit — measured, and a decision to make

The list style renders **one DOM level per round**, and the wasm stack gives
out on a deep chain. Measured now (list style, live :8480, v0.5.65+):

| session | rounds | list-style nodes rendered | wasm traps |
|---|---|---|---|
| `rewind` / `issue` / `webui_extend` | 32 / 30 / 27 | 32 / 30 / 27 ✅ | 0 |
| `Time inject` / `theme` / `essence` | 11 / 16 / 6 | 11 / 16 / 6 ✅ | 0 |
| **`Webui`** | **117** | **6** ❌ | 5 × `RuntimeError: memory access out of bounds` |
| **`alpha`** | **82** | **6** ❌ | 8 × same |

The app survives (the trap is caught at the wasm-bindgen closure boundary),
but the new tree never paints — the previous session's tree stays on screen.
The cut is between 32 and 82 rounds; it does **not** track payload size (the
four parse-broken sessions included a 16-round one), so it is a per-round
DOM/stack cost, not the serde limit.

**Style B draws all 117 rounds fine** — the flat `flow` projection is
precisely the answer to this, and is the reason to use it on the big
sessions. Fixing Style A (e.g. rendering a single-child *chain* as flat
siblings instead of nested `children`) would change Style A's markup, which
§10 pinned as "byte-for-byte"; it is **left for a decision** rather than
changed unilaterally.

#### A design consequence worth a decision

**A scene that fits its panel cannot turn.** The auto-fit (D4) leaves a short
line almost no horizontal travel — an 8-column tree is at most ~1.125× the
panel width — so the wheel has nothing to pan and a branch keeps the turn its
position under the light implies (e.g. `Time inject`: ~28° at the default
width, flattening only as it is pulled towards the centre). Long forked
sessions pan and turn as designed. If more motion is wanted for short trees:
raise the cell cap, skip the fit for short scenes, or make the *light* travel
instead of the scene. Not changed unilaterally.

#### Verification (the exact commands)

| command | result |
|---|---|
| `cargo test -p rushi-web` | **65 passed**, 0 failed |
| `cargo check --target wasm32-unknown-unknown --manifest-path web-leptos/Cargo.toml` | clean (pre-existing warnings only) |
| `trunk build` | ✅ success |
| `python3 e2e/flow_style_b_probe.py 8480` | **60 checks, 0 failed** |
| `python3 e2e/flow_check.py 8480` | 8 sessions, 0 problems |
| `python3 e2e/rewind_probe.py` | 140/140 (Style A untouched) |
| `python3 e2e/layout_probe.py 8480` | PASS |

`flow_style_b_probe.py` sections: **A** the switch (persisted, `list`
default) · **B** the 1:2 split and the x-only scroller · **C** the scene
(one dot per round, runs/hinges on the right lane, the lit live path, lane
geometry, no dot overlap, the 64 px pitch cap, connector rows) · **D** the
panel (D5 pre-select, the B2 text verbatim, the button inert on the current
round, a click selects and opens **no** dialog) · **E** the button → the one
dialog → cancel writes nothing · **F** the 117-round session (26 px floor,
drag pan, wheel, a horizontal delta left native) · **G** persistence, and
the list still draws afterwards · **H** the 3D (a ribbon per branch, real
`perspective` + `preserve-3d`, the proxy's maths checked against the CSS at
two offsets, flat under the light, `prefers-reduced-motion` → no transform)
· **I** the paint (accent pixels counted from real screenshots in **both**
palettes).

### 10.9 Round 2, part 1 — the flat list and the `flow` default (as built, v0.5.67)

Two of the round-2 items are landed and verified; the third — the orbital
scene — is **§12, a plan** (research, design, D-questions, steps), because it
needs the D-orb answers before it is worth building.

**1. Style A's wasm-stack blowup: a chain is now flat, not recursive.**
The list nested one `children` container per round (`node_view` recursed into
`.rw-kids`), so a straight chain of N rounds cost ~2N DOM levels. Measured on
the live app at v0.5.66:

| session | list-style `.rw-node` DOM nodes, pre-fix | post-fix | traps |
|---|---|---|---|
| `Webui` (117 rounds) | **6** | **117** | 5 → **0** |
| `alpha` (82 rounds) | **6** | **82** | 8 → **0** |
| `rewind` (32 rounds / 33 nodes) | 33 | 33 | 0 |
| `issue` (30) | 30 | 30 | 0 |
| `webui_extend` (27) | 27 | 27 | 0 |
| `theme` (16) | 16 | 16 | 0 |
| `Time inject` (11) | 11 | 11 | 0 |
| `essence` (6) | 6 | 6 | 0 |

All eight measured in the same sweep after the fix (8/8 full count, **zero**
`memory access out of bounds`); the six short sessions were already fine
before it, unchanged. The "before" numbers for `Webui`/`alpha` are the ones
that made the defect visible.

The failure was `RuntimeError: memory access out of bounds`: the app survived
but the tree kept the **previous** session on screen, i.e. History looked like
a stale copy. The cut sat between 32 and 82 rounds and did **not** track
payload size (the four serde-broken sessions included a 16-round one) — it was
a per-round DOM/stack cost.

**Fix**: `nodes_view(state, sess, Vec<RewindNode>) -> Vec<AnyView>` — a node
with a **single** child hoists that child to be the next *sibling* in the same
container, so recursion depth is the number of **forks** along a path, not the
number of rounds. `node_view` keeps rendering one node plus its fork children
(through `nodes_view`), so a forked tree's shape is unchanged, one indent level
per fork.

A pure chain therefore stops drawing as a staircase: every round of a run
shares one container, so a run is one flat column (the probe measures a single
left offset across 117 nodes) at one DOM level per fork.

The `byte-for-byte` pin from §9/§10 is **waived for this fix** at the user's
word ("按你说的修法做").

**2. The default History style is `flow`** (user: "默认值改为flow"), which
supersedes D2's "the list is the default". `read_view_mode` maps anything but
the stored `"tree"` to `flow`; the `[ list | flow ]` switch's active state and
the `rushi-rw-view` restore follow automatically, and `list` stays one click
away and persisted per browser. The rationale is exactly the two defects Style
B existed to fix: the scene draws every session measured, the recursive list
could not draw the long ones at all.

`e2e/rewind_probe.py` (Style A's probe) now pins `rushi-rw-view=tree` for
itself: a probe must *choose* the style it tests instead of inheriting a
default. `e2e/flow_style_b_probe.py` gained section **K** for the long chain
(117 rounds in the list style, no trap, nesting depth ≤ 5, one left offset,
the current round still marked).

#### Verification (v0.5.67)

| what | command | result |
|---|---|---|
| server logic | `cargo test -p rushi-web` | **65 passed** |
| frontend | wasm `cargo check` + `trunk build` | clean |
| Style A (pins the list style) | `e2e/rewind_probe.py` | **140/140** |
| Style B + the long chain (section K) | `e2e/flow_style_b_probe.py` | **66/66** |
| the scene payload | `e2e/flow_check.py` | 8 sessions / 0 problems |
| layout | `e2e/layout_probe.py` | PASS |

---

---

## 11. Rewind × compaction — the plugin side (plan, 2026-10-03)

Context: the kernel defect this plan depends on is fixed in
`rushi/docs/rewind-fork-design.md` section 11. Before that fix, a rewind
past a compaction boundary that was created *inside* the branch being
abandoned did two wrong things (measured on this session): the framing
item was the abandoned branch's handoff (`handoff/v4.md`) and every
active event was dropped, so the model resumed with no history and with
a summary of the branch the user had just left. With the fix the
projection uses the boundary on the active path: the history comes back
raw and the framing is the handoff in force at the target.

The plugin itself was correct — it appends one marker and renders the
tree — but it was silent about three things the user needs to see. This
section is the plan for those.

### 11.1 R1 — the server-side pre-check (pure, tested)

`bin/rushi-web/src/rewind.rs` already mirrors the kernel's
`active_ranges` (with the oracle test asserting equality). Add the same
treatment for the two guards the kernel applies *after* the mask:

```rust
/// Would the kernel ignore the marker this pick would append?
/// The P4 pair-stranding rule (docs/rewind-fork-design.md I3/P4) and
/// the settled-target rule (section 7) evaluated over the projected
/// context, i.e. exactly what `bin/assemble` decides.
pub fn rewind_verdict(events: &[Value], target_seq: u64, mode: &str)
    -> RewindVerdict;   // Ok / StrandsPair{missing} / NotSettled{...} / TargetMasked
```

- Pure function over the event slice: no I/O, so `cargo test -p
  rushi-web` covers it (the client crate cannot be host-tested — the
  reason the tree is server-side in the first place).
- Tests: a target mid-step (steer message) strands a call → `StrandsPair`;
  a settled `user_message` target is `Ok`; a target that is itself
  masked (inside an abandoned span) is `TargetMasked`; an
  `assistant_message` with outstanding calls is `NotSettled`.

**Implemented (v0.5.61)** — `rewind_verdict(events, target_seq, mode)`,
`verdict_with(..)` (the tree supplies its parsed markers/boundaries) and
`strands_pair(&[&Value])`, a port of the kernel's
`context_strands_pairs` (`bin/assemble/src/main.rs`), over the kept
region the *kernel's* rule selects (the boundary on the active path,
then the active ranges, then `seq >= first_kept_seq`).

Two deviations from the sketch, both recorded rather than silently
taken:

- **`TargetMasked` is not a verdict.** A target inside an abandoned
  span is a perfectly good pick — re-entering an abandoned branch is
  case C of the design doc, and the tree already shows that node as
  `abandoned`. The verdict answers one question only: *would the
  kernel's projection drop the marker?* A masked target is answered by
  the same strand test as any other.
- **`POST …/rewind` refuses with `409 Conflict` + the verdict in the
  body** (`{"ok":false,"verdict":…}`) and writes nothing; `200` means
  the marker landed. The low-level WS command path
  (`{"kind":"rewind"}`) stays permissive — it is the TUI-compatible
  transport, the kernel itself accepts every marker there, and that is
  exactly the case the post-hoc notice (11.2) exists for.
- `POST /api/sessions/{id}/rewind` runs it before appending. The
  verdict rides back in the response (and is exposed read-only in the
  tree projection so the client can disable the node without a POST).

Decision (user, 2026-10-03, D-C): a rewind that will not take effect
must produce a notice. The plan is **block at the node (pre-check) and
still notify after the fact** (11.2) — the pre-check cannot be perfect
(the log is hand-editable), so the post-hoc path stays.

### 11.2 R2 — the notice

Two surfaces, one message each:

- **Pre-dialog**: the confirm dialog names the consequence before the
  marker lands — e.g. *"This point sits inside the branch the previous
  rewind abandoned; the model will resume from the handoff in force
  there."* or, for a stranded pair, *"…would leave a tool call without
  its result, so the loop ignores it. Pick the round's first message
  instead."* with the buttons unchanged (Cancel / Rewind).
- **Post-hoc**: after a rewind, if the kernel ignored the marker, the
  plugin area shows it. The reliable source is the kernel, not a
  re-run of R1 in the client: the kernel already prints
  `assemble: ignoring rewind at seq N (target seq T): …` on stderr of
  the *loop*, which the webui cannot read. Two options:

  | option | mechanism | cost |
  |---|---|---|
  | A | the loop appends an `ext_status` marker (`id = "rewind.ignored"`, value = the marker seq/target) when `mask_active_path` drops a marker | one small kernel change, visible in the TUI too |
  | B | the webui re-evaluates R1 over the log after the loop runs and flags a disagreement | no kernel change, one re-implementation (already needed for R1) |

  Recommendation: **A**, with R1 as the pre-flight predictor. A is a
  3-line addition next to the existing `eprintln!` in `bin/assemble`,
  and it makes "did my rewind take effect" a fact in the log rather
  than an inference.

  **Implemented: B** (v0.5.61). B is the same rule as R1, which had to
  be written anyway, and it lives in the server module where
  `cargo test -p rushi-web` covers it — the kernel stays untouched for
  a client-side concern (the kernel-side `eprintln!` remains the
  loop's own record). The tree gains `ignored: [{seq, target_seq,
  mode, missing}]` (outermost first, a port of `mask_active_path`'s
  pop loop) and `tail_ignored: Option<IgnoredMarker>` — the log's last
  marker *is* one the kernel dropped, i.e. "your last rewind did not
  take effect". The plugin area renders it; `settled` becomes false
  for that log (the tail marker did not move the cursor).

  **One consequence that was easy to miss**, and is a real defect
  class of its own: a dropped marker must not move the tree's cursor.
  Before v0.5.61 `build()` let *every* marker move the cursor, so a
  log whose tail marker the kernel ignores showed a "you are here"
  one round too far back — the tree disagreed with the projection.
  The scan now skips the dropped markers (`ignored_markers` is
  computed once, before the scan) and the active path, the round
  states and every annotation come from the markers that survive.
  `a_dropped_marker_does_not_move_the_tree_cursor` pins it.

### 11.3 R3 — the node annotation ("what will be restored")

The tree projection gains one field per node:

```rust
/// How the context at this node is reconstructed (kernel rule,
/// docs/rewind-fork-design.md section 11).
pub enum Restore {
    Raw,                       // no boundary on the active path at this seq
    Framed { version: u64, from_seq: u64, to_seq: u64 },  // handoff vN + raw from_seq..to_seq
    Unresumable,               // the marker would be ignored (R1 verdict)
}
```

- Computed server-side from the same helper the kernel uses (the
  boundary on the active path), so the annotation is provably the kernel's
  answer rather than the client's guess.
- Rendered in the expanded view's node detail (top panel of Style B,
  and the tree's detail line for Style A): *"Resume: raw 1..x"* or
  *"Resume: handoff v3 + raw 6211..x"*.
- This is the user-visible form of D-D: a rewind to a round before the
  last compaction shows `Raw`, and the earlier rounds really are rebuilt
  from their original events.

**Implemented (v0.5.61)** — `Restore { Raw | Framed{version, from_seq,
to_seq} | Unresumable{missing} }` per node, from `restore_with` (one
parse of the markers/boundaries for the whole tree). Client:

- every node carries a `.rw-restore` detail ("raw history", "handoff v1
  + raw 1..4") and its tooltip ends in *", resumes from …"*;
- an `Unresumable` node gets the `.unresumable` class and a
  `not resumable` badge;
- the dialog names the source before the write (*"The context resumes
  from handoff v1 + raw 1..4."*), and for a blocked pick it explains
  the strand and **disables Rewind** — the dialog still opens, because
  a dead click tells the user nothing (the pick is refused by the
  dialog, not by silence).

### 11.4 R4 — probes, mirrors, docs

- `e2e/rewind_probe.py` (110 checks today, local-only per the account
  rule) gains:
  - a fixture whose log has a boundary inside an abandoned span (the
    session-`rewind` shape) → assert the tree marks `Framed{v3}` for the
    rounds before it and `Raw` for the ones after;
  - a stranded-pair fixture → assert the POST is refused, the dialog
    shows the notice, and the log gains no marker;
  - a "kernel ignored the marker" fixture (`rewind.ignored` marker) →
    assert the plugin area shows the notice.
- Mirror sync: `rushi-rewind` package — `server/rewind.rs`,
  `client/rewind.rs`, `client/*.css`, `install/TOUCHPOINTS.md`,
  `UPSTREAM`, README; probe stays local.
- `docs/rewind-plugin.md` (the design doc) gets the same section summary.

**Implemented (v0.5.61)** — a fourth fixture session, `rewindprobe4`:

```
1  user "round one"        5  assistant → tool_call c1    10 boundary v2 (fk 8)
2  assistant               6  user "steer"                11 rewind → 4
3  boundary v1 (fk 1)      7  tool_result c1              12 assistant
4  user "round two"        8  user "round four"           13 user "round five"
                           9  assistant                   14 assistant
                                                          15 rewind → 6  ← the kernel ignores it
```

`v2` is inside the span the rewind at 11 abandons (the session-`rewind`
shape); the marker at 15 would strand `c1`. Checks: the annotations
(`1 → raw`, `4 → framed v1 1..4`, `6 → unresumable c1`, `8 → framed v1
1..8`, `13 → framed v1 1..13`), the ignored list `[(15, "on", "c1")]`,
the tail notice, `409 + strands_pair` on a repeat pick **with no line
written**, `409 + not_settled`, the client's `.rw-restore` / badge /
tooltips / dialog (blocked + enabled), the plugin-area notice, and the
settled path (200, one line, cursor at 4, the old marker then masked
rather than ignored).

The probe is now **140 checks** (was 110), all green; the three
pre-existing rail checks gained the fourth session. `e2e/layout_probe.py`
still PASSes.

### 11.5 Verification

| step | command | result (v0.5.61) |
|---|---|---|
| server logic | `cargo test -p rushi-web` | **54 passed** (49 + `boundary_on_active_path_ignores_an_abandoned_boundary`, `verdict_flags_a_stranded_pair`, `ignored_markers_report_the_dropped_marker`, `a_mid_step_node_is_unresumable`, `a_dropped_marker_does_not_move_the_tree_cursor`) |
| frontend | `trunk build` | clean |
| probes | `e2e/rewind_probe.py`, `e2e/layout_probe.py` | **140 checks PASS**, layout PASS |
| end-to-end (kernel) | rewind this session to 6670 and to 6998, dump `bin/assemble` | framing = `handoff/v3.md`; `6211..<target>` present; no seq in `6671..9087` — proved with the kernel fix (`rushi/docs/rewind-fork-design.md` 11.4) |

## 12. The orbital flow view (plan, 2026-10-03)

**Goal (user):** *"scroll-driven character orbit rotation … 我需要树状图的分叉可以围绕会话主轴旋转"* — in the History scene the **branches must orbit around the session's main axis**, driven by scrolling, so that a session with many forks and a long history does not have to be crammed into one flat plane.

**Status: plan. Nothing in this section is built.** §12.2 is research (web + measured spikes in our own Chromium against the live app), §12.3 the design, §12.4 the decisions we need from the user, §12.5 the step plan, §12.6 how each step is verified, §12.7 non-goals, §12.8 risks.

### 12.1 The problem, concretely

Today's scene (§10.2, §10.8) is a **flat lane layout**: the main line is one
row (`--lane: 0`), every fork gets its own screen row (`--lane ±k`, pitch = a
percentage of the scene height), the rounds spread along **x** (`--cell`,
`--bx`), and the scene is a long horizontal strip. It reads well up to a
handful of forks. What does not scale is the **branch count**: lanes divide the
scene's height, so 20 forks means 21 rows of a few pixels each, and the
vertical extent of the picture — not the history — becomes the limit. The
history's length is already handled (x-scroll); the fork count is not, and it
is the dimension the user is pointing at.

The want, restated as a rendering problem: keep the trunk (the session's main
axis, and the thing that scrolls with the history) **exactly where it is**,
and give the forked chains a dimension that is not a screen row — an **angle**
around that trunk.

### 12.2 Findings

#### A. Measured in our Chromium, against the live app (spikes, 2026-10-03)

Spike sources (throwaway, local): `/tmp/orbit_spike.py`, `orbit_spike2.py`,
`orbit_spike3.py`; screenshots `/tmp/orbit-spike-*.png`.

**F1 — the native scroll-timeline machinery resolves, and the pivot is a registered custom property.** With `@property --phase { syntax: "<angle>"; inherits: true; initial-value: 0deg }` and `@keyframes { from { --phase: 0deg } to { --phase: 360deg } }` spread over `animation-timeline: scroll(nearest inline)` + `animation-range: cover`, the ring's `--phase` tracked the scroll exactly (`0deg` → `180deg` → `360deg` as `scrollLeft` went 0 → 50% → 100%), and its computed `transform` was the matching `matrix3d`. **Both** timeline arrangements work: `scroll(nearest inline)` (anonymous) and a named `scroll-timeline-name` + `scroll-timeline-axis: x`.

**F2 — the timeline must belong to the element or an *ancestor*.** The first spike failed because the scroller was a **sibling** of the animated element: the name resolved to nothing and nothing moved. Consequence for us: `#rw-flow-scroll` is already an ancestor of the track, so either form is available without disturbing §10.8's rule "the scroller is not the perspective element".

**F3 — two gotchas, both measured.** `animation-fill-mode: both` is **required**: at 100% the value held at `360deg` with `both` and snapped back to `0deg` with `none`. And `prefers-reduced-motion: reduce` does **nothing by itself** — the ring kept rotating under media emulation, so an explicit `@media` block is mandatory (this is the sharper version of D9).

**F4 — the geometry law for `rotateX(θ) translateY(-R)`** (a fin hinged on the axis, held out at radius R), measured on a 6-fin prototype:

| θ | projected row (`y`) | projected height | depth `z` |
|---|---|---|---|
| 0° | `-R` (above the axis) | 26px (full) | 0 |
| 60° | `-0.5R` | 11px (`26·cos60°`) | `-0.87R` (behind) |
| 90° | 0 (on the axis) | 0 (edge-on) | `-R` (behind) |
| 180° | `+R` (below) | 26px | 0 |
| 300° | `-0.5R` | 16px | `+0.87R` (**in front**) |

i.e. row = `-R·cos θ`, height ≈ `h·|cos θ|`, depth = `-R·sin θ`, with CSS's
`+z` **toward the viewer** — the near half of the ring is θ ∈ (180°, 360°).

**F5 — depth sorting and hit testing are the browser's job, and they are honest.** With two fins overlapping on screen at different depths (θ = 120° green vs θ = 240° orange, 90px tall, overlapping band 175×34px): at phase 0 the overlap painted **orange** and `elementFromPoint` returned the orange fin, with projected heights 39 vs 53; **rotating the ring 180° swapped both** (53 vs 39, painted green, hit green). A dot on a rotated fin is still hit-testable. So the orbit needs no z-index bookkeeping, no manual sorting, and clicks keep working.

**F6 — the flat projection is a one-line difference.** With the ring flattened, `transform: translateY(calc(-1 * var(--r) * cos(var(--th))))` put the same fins on the expected rows with full height (both fins at the same row for θ = 120°/240°, `cos` = −0.5). A reduced-motion / no-3D fallback is therefore a *projection of the same numbers*, not a second layout.

**F7 — the existing driver already exists.** §10.7/§10.8's `--rw-scroll` is written by exactly one write per frame (wheel, drag, `on:scroll`). The orbit's phase can be *derived* from it in pure CSS (`calc(var(--rw-scroll) * var(--deg-per-px))`), so the fallback path costs **no new JS per frame** and the native timeline becomes an optional replacement of the *source*, not of the transforms.

#### B. Browser support (MDN browser-compat-data, fetched 2026-10-03)

| feature | Chrome | Safari | Firefox |
|---|---|---|---|
| `animation-timeline` / `animation-range` / `scroll-timeline*` | 115 | 26 | **`preview`** (not stable) |
| `@property` (registered custom properties) | 85 | 16.4 | 128 |
| CSS `sin()` / `cos()` / `atan2()` | 111 | 15.4 | 108 |
| `perspective`, `transform-style`, `backface-visibility` | 36 | 9 / 9 / 15.4 | 16 |

Read together: **the native scroll-timeline path is a Chromium + Safari enhancement; Firefox needs the JS proxy** (this is the crisp, current version of D9 — a blog claiming "Firefox shipped it in 132" is wrong, BCD says `preview`, and the plan trusts BCD over blogs). The **trig is broader than the timelines**, so any projection computed with `sin`/`cos` is safe as the *base* layer.

#### C. Prior art

A 3D carousel is `perspective` on a stage, a ring of items each
`rotateY(θ) translateZ(R)`, and the ring's own rotation driven by scroll or a
timer (SitePoint's classic build; the "scroll rotate gallery" pattern). Ours is
the same construction rotated 90°: the items orbit around the **x** axis
(`rotateX`), because our turntable's axis is the horizontal trunk, not a
vertical one — and the axis is shared by every branch, which is exactly the
user's "围绕会话主轴旋转".

Sources: MDN `animation-timeline` / `animation-range` / `transform-style`;
`mdn/browser-compat-data` (raw JSON for `animation-timeline`,
`animation-range`, `scroll-timeline`, `scroll-timeline-axis`,
`at-rules/property`, `types/sin`, `types/cos`); WebKit's animation-range
cheatsheet (Jul 2025); web.dev on CSS trig; SitePoint "Building a 3D rotating
carousel with CSS and JavaScript"; the `scroll(nearest inline)` +
`animation-range` + `prefers-reduced-motion` tutorial the user pasted.

#### D. What the payload already gives us (no server change)

`Flow.branches` has, per segment, `root`, `lane`, `parent_lane`, `from_x`,
`to_x` (the main line included, `lane` 0), and `Flow.nodes` has `x`, `lane`,
`main`, `seq`, `current`. So the client can compute every number the orbit
needs — the hinge column (`from_x`), the fin's own x span (`from_x..to_x`),
the branch list (everything with `lane ≠ 0`), and which branch holds the
current node — **from the response it already fetches**. The orbit is a
client-side projection of the same `flow` object, which keeps §10.2's
"one fetch, one cache, one invalidation path" intact. If a later step wants
server-side correctness (unit-testable geometry), the *server* could also emit
`--theta` per branch — that is a D-orb question, not a requirement.

### 12.3 Design — the orbital scene

**The stage.** Unchanged from today: `#rw-flow-scroll` scrolls **x only** and
is not the perspective element; `#rw-flow-track` is the perspective stage. New
inside it:

```
#rw-flow-track                 perspective: 1400px;  transform-style: preserve-3d
  .rw3-trunk                   the main line: runs, dots, the current marker — untouched
  .rw-orbit                    transform-style: preserve-3d;
                               transform: rotateX(var(--phase))
    .rw-fin (per branch)       transform-origin: 0 50%;
                               left: <fork column x>;  transform: rotateX(var(--theta)) translateY(calc(-1 * var(--r)))
      .rw-fin-body             the branch's own run/dots, along the fin's x span
      .rw-fin-label            counter-rotated (see "labels")
```

**The numbers.** `R` (the ring radius) and the per-fin `--theta` are written at
**layout** time by `sync_scene_metrics`, next to today's `--bx`/`--halfpw`/
`--denom` (mount, tree change, selection, resize) — never during a scroll.
`--theta_i = (i − align) · 360/N` for the N branch fins, ordered as
`Flow.branches` already orders them (left to right by fork column, stable
across reloads). A scroll frame keeps writing **exactly one value** — the
existing `--rw-scroll` — and `--phase` is pure CSS on top of it:

```css
.rw-orbit { transform: rotateX(calc(var(--rw-scroll) * var(--deg-per-px))); }
```

so the *rotation* costs no JS at all and the scene keeps its single-write
contract.

**Why this fixes the density problem.** The two growing dimensions are
decoupled: **history length → x** (the trunk's own scroll, exactly as today)
and **branch count → angle** (the ring's N slots). A 117-round session with 30
forks no longer needs 30 screen rows; it needs one ring, and the scroll brings
any fin to the front. That is the honest answer to "长历史不必挤在一个平面里".

**Where the fins hinge.** At the trunk column where the branch forks
(`from_x`), so the ring is a *radial* drawing of the same tree: a fin starts on
the trunk and extends outward. Fins from different forks do not share a column,
so they overlap in x only when their spans overlap — and when they do, F5 says
the browser paints and hit-tests the nearer one correctly.

**Depth cues** (what makes it read as 3D — all of them free):
perspective already scales the near fins up and the far ones down (measured 53
vs 39 px); we add a `cos`-derived opacity (`--depth: cos(var(--theta) +
var(--phase))`) so the front-facing fins are bright and the far ones dim, and
we let the browser sort painting and hit testing (F5). No `z-index` games.

**Labels.** Two rules, because a rotated label is unreadable: (a) the branch
**label layer counter-rotates** — `rotateX(calc(-1 * (var(--theta) +
var(--phase))))` — so its text is always flat to the viewer; (b) the fin at the
front is the readable one, and the panel below (`#rw-detail`, D6) remains the
text surface for everything else. Note that (a) needs `--phase` as a **value**,
which is why the D-orb-4 recommendation favours the value-driven phase: the
purely-transform native path can rotate the ring but cannot counter-rotate a
label.

**Alignment at rest.** The **current branch** (the branch holding the node with
`current: true`, else the selected node's branch) sits at the front, i.e. its
effective angle is 0°; `--phase`'s zero is set from that. Clicking any fin's
dot re-aligns it to the front (and selects the node, as today). So the picture
always answers "where am I" without reading any text.

**Many branches.** N ≤ 6 → slots of ≥ 60° are comfortable. Beyond that the
ring still works, but adjacent fins crowd; the front-alignment rule (scroll or
click) is what keeps it usable, plus the tangential dimming. The degenerate
cases are the interesting ones: **0 branches** → nothing to orbit, the ring
stays flat and the scene is exactly today's scene (no regression for the short
sessions §10.8 measured); **1 branch** → a degenerate ring; the proposal is a
gentle ±25° swing rather than a full turn (D-orb-9).

**The flat projection** (`prefers-reduced-motion: reduce`, or a forced `.flat`
class for probes) is F6's one-liner: ring `transform: none`, each fin
`translateY(calc(-1 * var(--r) * cos(var(--theta))))`, full height, upright
labels, depth dimming kept (it is a `cos`, so it still works). Non-goal: no
attempt to animate anything in this mode.

### 12.4 D-questions (each with a recommendation in bold)

**These block O2.** O1 (registering the phase plumbing) can start before the
answers. I recommend answering them in one pass, as we did for D1–D9.

* **D-orb-1 — the radius `R`.** A percentage of the scene's height (like the
  lane pitch today), a fixed px, or auto-fit so a fin's far end just fits?
  **Recommendation: a CSS var written at layout time, `R ≈ 0.36 × scene
  height`, capped by the auto-fit rule; no user control (D4 keeps auto-fit +
  drag-to-pan, no slider).**
* **D-orb-2 — the angular law.** Full 360° ring with evenly spaced slots vs a
  bounded arc (e.g. ±70°) with the extra branches queued behind the front.
  **Recommendation: the full ring (it is what "orbit" means and it scales to
  many branches); evenly spaced slots, order = `Flow.branches`.**
* **D-orb-3 — what turns.** Branches only (the trunk fixed) as the user asked,
  or a small camera tilt for extra depth cueing? **Recommendation: branches
  only; the trunk is the one stable thing in the scene, and stability is what
  makes the orbit legible.**
* **D-orb-4 — the phase driver.** (a) derive `--phase` from the existing
  `--rw-scroll` proxy (works everywhere, one write per frame, can drive label
  counter-rotation); (b) a native `animation-timeline` + `@property`
  (compositor-smooth, Chromium/Safari only, cannot counter-rotate labels);
  (c) both behind `@supports`. **Recommendation: (a) as the base and (c)
  afterwards only for a per-fin `view(x)` reveal if it earns its keep — one
  driver, one meaning, the same rule as D7.**
* **D-orb-5 — labels.** Counter-rotate (always flat, needs the phase as a
  value) vs let them turn with their fin (simpler, unreadable at steep
  angles). **Recommendation: counter-rotate the label layer only; the fin's
  geometry keeps turning.**
* **D-orb-6 — the far side.** Keep the whole ring visible (dimmed) vs hide the
  back half. **Recommendation: keep it visible, dimmed by `cos` — the ring
  turning into view is the whole point of a scroll-driven orbit.**
* **D-orb-7 — alignment.** Current branch at the front (recommended), a fixed
  ring with only the scroll turning it, or click-to-align. **Recommendation:
  current branch at rest + click-to-align; the scroll only adds phase on top.
  This is the same "stay anchored to the current node" rule as D5.**
* **D-orb-8 — the scroll mapping.** One full turn per scene width? per N
  fins? per fixed px? **Recommendation: a fixed angular rate per px
  (`--deg-per-px` ≈ 360° / (1.5 × scene width)), so the gesture feels the
  same in every session, with the alignment rule always winning at rest.**
* **D-orb-9 — degenerate sessions.** 0 branches (nothing to orbit) and 1
  branch (a ring of one). **Recommendation: 0 → flat, exactly today's scene;
  1 → a gentle ±25° swing so the scene still feels alive but stays readable.**
* **D-orb-10 — where it lives in the UI.** The flat lane layout as the
  reduced-motion projection (recommended: the orbit replaces the lane layout,
  the lane layout becomes the fallback), vs a third style next to `[ list |
  flow ]`, vs a per-session preference. **Recommendation: keep two top-level
  styles — `list` and `flow` — and make the orbit *inside* flow the only
  layout; the lane layout survives only as the flat projection, so the switch
  keeps meaning "text or tree", not "3D or not".** If the user wants to
  compare, a `.flat` toggle is a one-line probe affordance, not a UI switch.

### 12.5 Step plan

| step | what | why in this order |
|---|---|---|
| **O1** | the phase plumbing, no visual change: `@property --phase` (and the `--theta`/`--r`/`--deg-per-px` vars) written at layout time in `sync_scene_metrics`; `--phase` derived from `--rw-scroll` in CSS; still no ring | it can be verified numerically (computed styles) while the scene looks exactly as it does now |
| **O2** | the ring: `.rw-orbit` wrapper with `preserve-3d`, fins hinged at `from_x`, radius `R`, `--theta` per branch; the lane layout demoted to the flat projection | the first visible step; decidable only after D-orb-1/2/3/10 |
| **O3** | the alignment rule: `--phase`'s zero = the current branch at the front; clicking a fin re-aligns | answers "where am I" without text |
| **O4** | depth cues: `cos` opacity, the far-side policy, `backface-visibility` if the far fins' backs look wrong | the measured pieces (F4/F5) |
| **O5** | labels: the counter-rotated label layer, the panel as the text surface | needs the phase as a value (D-orb-4/5) |
| **O6** | the flat projection + `@media (prefers-reduced-motion: reduce)`, and — only if it pays — `@supports (animation-timeline: view(x))` for the per-fin reveal | §12.2 F3 says the media query is mandatory; the `@supports` bit is optional |
| **O7** | probes, docs (this section's as-built record + `docs/rewind-plugin.md`), mirror sync | same discipline as B9 |

Every step is a build + probe cycle; each one is independently revertible
(the orbit is behind its own wrapper and vars).

### 12.6 Verification

**Commands (each step):** `cargo test -p rushi-web` (server, 65 today —
unchanged unless D-orb-2's server-side variant is chosen), wasm
`cargo check`, `trunk build`, then the probes on the live app.

**Probe plan** — a new section **L** in `e2e/flow_style_b_probe.py`, plus the
existing A–K kept green:

| # | check | how |
|---|---|---|
| L1 | the ring exists and is a 3D context | `getComputedStyle(.rw-orbit).transformStyle == preserve-3d`, the stage's `perspective != none` |
| L2 | one fin per non-main branch | fin count == `branches.filter(lane != 0).length` |
| L3 | the geometry law (F4) | the θ≈0 fin's box is **above** the axis, the θ≈180 one **below**, and their `|y − axis|` are within a few % |
| L4 | the depth law | the near fin's projected height > the far fin's, by the measured ratio (~53/39) |
| L5 | scrolling turns the ring | after a wheel/proxy scroll, `--phase` changed and L4's ordering **swapped** when the phase passed 180° |
| L6 | the trunk does not move | the trunk's rect and the main-line runs are bit-identical before/after the scroll |
| L7 | the current branch is at the front at rest | its effective angle ≈ 0 (±5°) after entering the scene |
| L8 | clicking a fin aligns it and selects its node | the fin's root node becomes `selected`, the ring re-aligns |
| L9 | dots on rotated fins are clickable | `elementFromPoint` at a rotated dot returns the dot (F5) |
| L10 | the flat projection | with `.flat`, each fin's row ≈ `-R·cos θ` (exact, no perspective), full height, no transform on the ring |
| L11 | reduced motion | with `Emulation.setEmulatedMedia(prefers-reduced-motion: reduce)` the ring's transform is `none` and nothing changes while scrolling |
| L12 | the long chain + many branches | open `Webui` (117 rounds) and the fork fixture: fin count, no traps, node count unchanged, a paint check in both palettes (like section I) |

**What would falsify the design** (kept explicit, so the probe can prove us
wrong): if L4/L5's height ordering does not invert when the ring passes 180°,
the ring is not a real 3D context; if L6 fails, the trunk is being transformed
with the ring; if L9 fails, the orbit has cost us the primary interaction; if
L10 cannot be computed from `cos`, the flat fallback needs a second layout
(and D-orb-4's option (b) gets less attractive).

### 12.7 Non-goals

No WebGL, no three.js — this is CSS 3D plus one number, or it is not worth it.
No 3D in the **list** style (Style A stays a flat text tree; the flattening fix
above is the whole of its round-2 work). No rotation of individual **rounds**
inside a branch — the unit that orbits is a **branch** (a fin); its own rounds
stay on its fin. No new server route or payload field unless D-orb-2 asks for
it. No change to auto-fit/pan/zoom (D4), to the detail panel (D6/D8), or to
the wheel contract (D7): the orbit *consumes* the same single value those
decisions produce. And no `"before"`-mode or TUI work (still v2).

### 12.8 Risks and mitigations

| risk | mitigation | evidence |
|---|---|---|
| labels unreadable while rotated | counter-rotated label layer + front alignment; the panel is the text surface | F1 (the property path), D6 |
| many branches crowd the ring | slots + `cos` dimming + front-alignment on scroll/click | F4/F5 (the ordering stays true at any N) |
| the 3D fights the x-scroller | the scroller stays *outside* the perspective element (unchanged §10.8 rule); the rotation lives on the ring | F2 (ancestor timeline) |
| painting/hit-test surprises | none expected: the browser sorts by depth and hit-tests accordingly | F5 (measured, including the 180° swap) |
| Firefox | the base path is the JS-driven value, which Firefox has | §12.2 B (BCD: Firefox is `preview` for timelines) |
| old browsers without `@property` | the proxy *writes* the value per frame, so no property registration is needed at all on that path — registration only matters if we adopt the native timeline (D-orb-4 (b)) | F1, B |
| cost per frame | the ring's transform is one CSS calc over a value we already write; the fins are static transforms written at layout time | F7, §10.7 |
| 117-round scene + many fins | the ring adds one wrapper level and one transform per fin; the node count is unchanged | §10.9's measurement |

### 12.9 As built (2026-10-03, v0.5.68)

The ring is in. `bin/rushi-web/src/rewind.rs` emits the ring's numbers and
`web-leptos` turns them into planes; the decisions D-orb-1..10 all landed,
with five refinements that only showed up once the thing was turning.

**Server** (`v0.5.68`, 3 new tests, 68 total green)

* `Orbit { step_deg: 30, arc_deg: 60, fins }` on `Flow` (`ORBIT_STEP_DEG`,
  `ORBIT_ARC_DEG`): the client does not decide how many branches fit the
  window — `60/30 + 1 = 3` are visible, the rest queue (D-orb-2).
* `FlowBranch` gained `fin: Option<u32>` (the slot, in branch order; `None`
  for the trunk — D-orb-2's "bounded arc" needs a *stable* order, and the
  leftmost branch is the one at the front at rest), `hinge_x` (the column of
  the round the branch left, i.e. `x` of its parent) and `seqs` (the rounds
  that belong to *this* branch — the fin's own beads).
* `place()` now tracks the branch being written, so a nested fork's rounds go
  to its own branch and not to the trunk's.

**Client** (`web-leptos/src/rewind.rs`)

* The scene is `{ trunk edges } { trunk nodes } <div class="rw-orbit">{ fins }</div>`.
  A fin is the branch's rounds plus the runs that feed them (`fin_of` maps a
  round to its fin), so nothing is drawn twice and nothing is left out.
* `sync_scene_metrics` (layout time: mount, tree, selection, resize) writes
  `--axis` (0.62 × height), `--r` (auto-fit: `axis − 16px − 13px`, floor 34),
  `--dpp` (360 / 1.5 × width — D-orb-8) and `--rw0` (the layout-time
  `scrollLeft`, which is what makes "rest" mean "aligned" — D-orb-7).
* `flow_scene` writes `--align` (selected round's fin → current round's fin →
  0) and `--step`/`--arc` from the server; each fin carries `--fin`/`--hinge`.
* `.rw-orbit.solo` when there is exactly one fin (D-orb-9, below).

**CSS** (one section, in `style.css`)

* `--phase: calc((var(--rw-scroll, 0) - var(--rw0, 0)) * var(--dpp, 0.3))` —
  a pure CSS calc over the one value a scroll frame writes (D-orb-4). No
  `@property`, no `animation-timeline`: the proxy path is the only path, and
  the native timeline was not adopted (it would have needed a second source
  of truth for a value we already have).
* A fin is `transform: translate3d(0, 0, over·−22px) rotateX(a)
  translateY(−r)` about the hinge point on the axis, with
  `--a: clamp(-arc, theta + phase, arc)` and
  `--over: max(0, eff − arc) + max(0, −eff − arc)`.
* `.rw-orbit.solo .rw-fin { --eff: calc(sin(phase·1deg) · 25) }` — the
  D-orb-9 swing, one line, no server field.
* `#rw-flow-track.flat` and `@media (prefers-reduced-motion: reduce)` unfold
  the same ring onto rows: `translateY(−R·cos a + 0.12·R·sin a)` (the sine
  term only splits the symmetric pair so the two ends can be told apart).
  The media query additionally pins `--phase: 0`, which is what makes it
  *static*: without it the rows would still slide with the scroll.

**Five things the plan got wrong, and what we did instead**

1. **The arc alone is not enough.** Clamping `theta` to ±60° piles every
   parked branch onto one row: they overlapped into mush. The parked fins
   are therefore also pushed back in depth by `over·22px` and faded by
   `1 − over/70`, which leaves a receding stack at each end of the arc —
   D-orb-2's queue, now visible.
2. **`--eff` is the wrong angle for the flat projection.** The first build
   unfolded `cos(eff)` (unclamped) while the 3D build used `cos(a)`
   (clamped), so parked fins landed *below* the axis (measured: −189px off,
   exactly `R`). Flat uses `--a`.
3. **The node's own transform had to move.** Style B centred the whole node
   box (dot + number) on the row, which floated every bead 6.5px above the
   run it sits on — invisible until the beads had to sit *on* a ring. The
   base rule is now `translate(-50%, -calc(dot/2))` with the counter-rotation
   pivoting on that bead, and C7/C9/C10 read the bead, not the box.
4. **A `prefers-reduced-motion` block that only drops the 3D is not
   static.** Pinning the phase is the actual mechanism.
5. **`.rw3-hinge` is dead.** Nothing leaves the axis any more, so there is no
   lane gap to bridge; the rule is gone and the mirror's canaries now look
   for `.rw-orbit`/`.rw-fin`.

**Verification** (`e2e/`, local only)

* The deep checks live in a **new, self-contained probe**,
  `e2e/orbit_probe.py` (34 checks, L1–L12), not as section L of
  `flow_style_b_probe.py` as §12.6 assumed: the live sessions cannot exercise
  the ring at all (only `Time inject` forks, its one branch is four columns
  long and the scene never scrolls), so the probe writes its own fixtures and
  starts its own server — seven fins off six early rounds plus a nested fork,
  a sixty-round trunk, a one-fork and a no-fork session. It recomputes the
  browser's own projection (rotateX + the perspective divide) and compares it
  to the rects the browser painted, to 3px.
* `flow_style_b_probe.py` keeps A–K green and gained a shorter section H
  (structure + the live one-fin swing) — 66/66.
* `flow_check.py` now asserts the ring's invariants against every live
  session: `orbit.fins == numbered fins`, slots `0..n-1`, the trunk is not a
  fin, `hinge_x` is the parent's column, the trunk branch carries exactly the
  main line, no round is on two fins — 8 sessions, 0 problems.
* `cargo test -p rushi-web` 68; wasm `cargo check` clean; `trunk build` clean;
  `rewind_probe.py` 140; `layout_probe.py` PASS.

**Measured on the live app** (what the laws predicted, to the pixel)

| what | measured |
|---|---|
| every fin's centre row | `axis − R·cos(theta)`, then the perspective divide — **within 0.0px** for all six fixture fins |
| the trunk through a full turn | beads bit-identical, transforms still plain 2D matrices (D-orb-3) |
| the arc's queue | 3 fins inside ±60°, the rest parked, ordered by depth, dimmer and smaller |
| a rotated bead | `elementFromPoint` returns the bead itself (D-orb-5/§12.2 F5) |
| the flat projection | rows to `<1px` of `−R·cos`, no 3D anywhere |
| reduced motion | rows do not move while the scene scrolls |
| a lone branch | flagged `.solo`, swings on `25·sin(phase)` |
| a nested fork | renders as its own fin (no traps) — the one shape with no drawn link to its parent, because its hinge is a column inside another fin |

**Non-goals, kept.** No WebGL, no per-round rotation, no orbit in the list
style, no new route, and the wheel contract (D7) untouched: the orbit reads
the same `--rw-scroll` the pan already produced.

### 12.10 Two defects the live app found (2026-10-04, v0.5.69)

The user's report — *"rewind 视图怎么转动？滚动没用"*, then *"rewind 会话有很多条分支啊"* —
was two independent bugs, and both were invisible in every probe, because the
probes' own fixtures are neither short nor half-written.

**Defect 1 — the P4 guard fires on an in-flight call, so a running session's
tree went flat.** `build`/`ignored_markers` are an exact port of
`bin/assemble`'s `mask_active_path`, which pops the outermost rewind marker
while the masked context strands a tool pair. The kernel only ever assembles
**between** turns — by then every call of the previous turn has its result —
while the plugin projects **on every request**. So for the whole time the
agent is inside a tool call (i.e. nearly always, when a user looks) the
in-flight call read as a strand, the loop popped *every* marker, and the log
was re-projected linear: the branches vanished from the screen while the
kernel's own context kept them.

Evidence, on the session named `rewind` (4 markers: 8910→6998, 8990→6670,
9087→6670, 14080→6670): with one of *my* tool calls in flight it projected
`37 rounds · 0 fins · ignored 4`; truncating the log to just before that call
projected `4 fins · 5 branches` (trunk 1..20; 6998 lane −1, 8911 lane −2,
8991 lane +1, 9088 lane +2 — all four off round seq 6670) with `ignored: []`.
The branches the user remembered were real; both my readings and their view
had been taken mid-call.

Fix: `build_live` / `rewind_verdict_live` (the handlers pass
`st.loops.is_running(id).await`). An id that is unpaired **anywhere in the
whole log** is *pending*, not stranded, while the loop is alive — a pop can
only bring back a counterpart that exists. The exemption is narrow: a pair
the **mask** splits (the call kept, its result in the log but outside the
active ranges) still strands, and with a dead loop (crash, settled log) the
projection is the kernel's rule again, bit for bit. **3 new tests**
(`an_in_flight_call_does_not_drop_markers_while_the_loop_is_live`,
`a_masked_but_logged_pair_still_strands_while_live`,
`a_pick_is_not_refused_for_an_in_flight_call_while_live`) → `cargo test -p
rushi-web` **71**.

**Defect 2 — most scenes cannot scroll at all, so the phase was frozen.**
`--phase` reads `--rw-scroll` = `scrollLeft`, and `#rw-flow-track` is
`(cols+1) × clamp(26px, 100cqw/cols − 4px, 64px)` wide: it overflows its
panel only for `cols ≤ 16` (the proportional regime) or `cols ≥ ~46` (the 26px
floor). For **17..45 columns the track fits exactly**, so `scrollLeft` is
pinned at 0 for ever and the wheel *and* the drag were dead, not subtle.
Measured with a real `Input.dispatchMouseEvent` wheel on a 4-fin / 21-round
session: `scroll [0, 1200, 1200]`, `--rw-scroll` `"0"`, and all four fins'
`transform` and `--eff` unchanged after 4× `dy=+120` and 2× `dx=+120`. The
live forked sessions sit exactly in that band (`Time inject` 8 columns,
`rewind` 37); only `alpha` (82) and `Webui` (117) can scroll, and neither has
a fin — which is why the ring looked dead everywhere.

Fix (`web-leptos/src/rewind.rs`): `--rw-scroll` is `scrollLeft + turn`, the
*turn* being the part of a gesture the track could not take — accumulated from
the wheel's overshoot at either end, set continuously from the pointer's own
wish while dragging. A scene that can pan takes the whole delta (nothing
changes: the pan and the ring are still the same number, one value per frame);
a scene that cannot pan now turns the ring on the spot, which is what D-orb-4
needed all along. The turn resets with `--rw0` at every **layout point**
(`relayout_scene()`: new scene, new selection, resize, session switch), so
"at rest = aligned" (D-orb-7) still holds — and a re-measure that is *not* a
layout point (`on:pointerdown`) no longer moves the ring.

**Open, deferred at the user's request** (they are checking the result by
hand): the two new probe checks — a fixture where the wheel turns a ring whose
scene fits, and a `loop.pid` toggle proving the pending rule end to end — plus
a re-run of `rewind_probe` / `orbit_probe` / `flow_style_b_probe` /
`flow_check`. Until those run, the browser-level verification for this round
stands at the API-level checks above; the unit tests and the builds are green.

## 13. The cone: a tree-shaped flow scene (proposed 2026-10-04, v0.5.70)

User request, verbatim: *"树状视图做优化 1. 树状图无需一定是横向的直线，一般来说主路径可以是一条到底的直线，但分支可以是在圆锥面分布，更符合树状图的实际意义 2. 节点之间的连线一定是直线，不能有拐弯 3. 每一条支线无需做背景彩带，点线简洁连接即可"* — followed by *"调研现状，列改进计划给我看"*, so this section is
a **proposal**: the D-cone-* decisions below are open until confirmed.

### 13.1 What the flow scene actually draws (measured, not remembered)

Dumped from the live scene (`rewind`, 24 columns, 4 fins, 1200×418 panel,
`--axis` 259px, `--r` 230px):

| element | geometry (measured) |
|---|---|
| trunk | 23 `.rw3-seg.main` bars + 24 beads, every bead at row 253-259 (= the axis), 46px apart — **a straight bead line**, as asked |
| a branch | `.rw-fin`: a 26px-tall **plane** with `background: linear-gradient(...)`, hinged at the fork column (`left: (hinge+0.5)·cell`, `transform-origin: 0 50%`), turned by `rotateX(a)` and lifted `translateY(−230px)` |
| the branch's beads | all on **one row inside that plane** (fin 0: rounds 6998..8867, every bead at y=23) — i.e. every branch runs **parallel to the trunk at a constant radius**: the branches form a **cylinder** around the axis, not a cone |
| the branch's own runs | `.rw3-elbow` bars inside the plane, e.g. `6670→6998` at `[x=575 (= the parent's column), y=28, w=46]` |
| parent link | **not drawn at all**: the bar starts at the parent's *column* but travels at the branch's *row*, so the 230px from the trunk bead up to the branch is empty — the branch floats (this is the gap the cone's straight line should fill) |
| the parked queue | `|eff| > 60°` is clamped to the arc, pushed `−22px` in z per degree over and faded (fin 3: `eff` 90° → parked at 60°, beads 8px instead of 13px) |
| the light | `.rw3-sheen`, a 7%-opacity vertical gradient pinned to the panel (not per branch) |

So: nothing *bends* today — but nothing links a branch to its parent either,
and the branch's own shape is a rectangle, which is what makes the picture
read as a ring of flags rather than as a tree.

### 13.2 The three asks, as geometry

1. **主路径一条到底的直线，分支在圆锥面分布.** Keep the trunk exactly as it is.
   Replace "a plane parallel to the trunk at radius R" with **a straight ray
   that leaves the fork bead**: the branch's *i*-th round sits at
   `(x = column, r = i·q)` — ahead along x and further out from the trunk with
   every round. Points of that form are collinear in 3D (`r` grows linearly
   with the round index), so the branch is one straight line on the surface of
   a cone whose apex is the fork point and whose axis is the trunk.
2. **连线一定是直线.** One line per branch — and it starts **exactly at the
   parent's bead** (the container's origin is `(hinge column, axis row)`,
   which *is* where the parent's bead sits), so the parent link and the
   branch's own spine are the same straight segment. No elbow, no separate
   hinge, no bend anywhere.
3. **去掉支线彩带.** Delete `.rw-fin`'s gradient band and its inset shadow.
   A branch becomes: **one straight line + its beads**. (`.rw3-sheen` is a
   panel-wide light, not a per-branch ribbon — D-cone-5.)

### 13.3 The numbers

* `q = (axis − margin − dot_r) / max_branch_len` — the D-orb-1 fit rule,
  generalised: the **longest** branch's tip just fits the panel, and every
  shorter branch gets the same slope (so the branches are parallel *in cone
  terms*, which is what makes them read as one tree).
* Slope in cells `s = q / cell` (measured example: `axis` 259, `margin` 16,
  `dot_r` 6.5, longest branch 7 rounds → `q ≈ 34px`, `cell` 46px → `s ≈ 0.73`,
  a ≈ 36° cone half-angle).
* Per bead: `--out: <i>` (render) and per branch `--q: <px>` (layout) — the
  CSS does `translateY(calc(-1 * var(--out) * var(--q)))`, so a scroll frame
  still writes exactly one value and a layout writes the pixels.
* Azimuth, turn, park, fade, `--align`, `--phase`, `--eff`: **unchanged**
  (the branch container keeps `rotateX(a)` about the axis with
  `transform-origin: 0 <axis row>`).
* Nested forks: nest the branch containers (a branch inside a branch is a
  child container positioned at its parent bead's local offset), so a nested
  branch's line also starts at its parent's bead — today it has no link at all.

### 13.4 Decisions (D-cone-1..7 — **all approved by the user**, 2026-10-04)

The user approved every proposal below verbatim; D-cone-7 was corrected
during the review (a nested branch fans **relative to its parent**, not in
its own absolute slot), which is what shipped — see §13.6.

* **D-cone-1** the cone applies to branches only; the trunk stays a straight
  bead line on the axis. (Proposed: yes.)
* **D-cone-2** the slope: `R / longest branch` (uniform, auto-fit) or a fixed
  slope with the far tip clipped? (Proposed: the auto-fit.)
* **D-cone-3** the branch's x step: keep one column per round (out and along,
  as today) or compress it so the cone is steeper and eats less width?
  (Proposed: keep the column — the x axis stays "history", and the tree reads
  as branches leaning out rather than as a second time axis.)
* **D-cone-4** the line's start: the **parent's bead** (one straight segment,
  proposed) or a point on the axis under the fork column?
* **D-cone-5** `.rw3-sheen`: keep (proposed — it is a light, not a ribbon) or
  drop it too?
* **D-cone-6** keep the park-and-queue treatment for `|eff| > arc`, and do the
  nested containers of D-cone-4's last bullet? (Proposed: yes to both.)
  *Sibling* branches are the simple case and need nothing special: every
  branch whose parent is on the **trunk** starts its line at that parent's
  bead and points in its own slot's direction (`--step` 30° apart, clamped to
  the ±60° arc). The nested container is about a branch whose **parent is
  itself on a branch** — measured in the live `rewind` session: fin 1
  (`8911`, edge `6998→8911`) has its line start at `[621, 71]` while its
  parent bead `6998` sits at `[621, 29]` on fin 0: today every branch is a
  flat sibling inside `.rw-orbit`, positioned in **trunk** coordinates, so a
  nested branch's radius has nothing to do with its parent's (it comes out
  *inside* it, 42px short, and never touches it). Nesting the container fixes
  exactly that.
* **D-cone-7** a nested branch's direction: a small fan **relative to its
  parent** (proposed — e.g. ±30° off the parent's plane, so a fork off a
  branch reads as a branch off a branch) or its own absolute slot?

### 13.5 Steps once the decisions land (O-cone-1..6)

1. **O-cone-1** client: per-branch container (nested), per-bead `--out`, one
   line element per branch, `--q` written by `sync_scene_metrics` (it already
   computes `--axis`/`--r`; `q` is one more division by the longest `seqs`).
2. **O-cone-2** CSS: drop `.rw-fin`'s background/shadow and the per-edge
   `.rw3-elbow` bars; add `.rw-branch`/`.rw-br-line`; keep the beat rules
   (`--theta`/`--eff`/`--a`/`--over`) verbatim; the flat and reduced-motion
   blocks drop the 3D but keep the cone's diagonal.
3. **O-cone-3** labels: unchanged (counter-rotated about the bead, upright).
4. **O-cone-4** server: **no change needed** — `fin`, `hinge_x`, `seqs` and
   the orbit constants already carry everything; the slope is pixels.
5. **O-cone-5** tests: the ring's unit tests keep their logic (fins, hinges,
   attribution); `orbit_probe`'s geometry section must be rewritten for the
   ray (it recomputes the old parallel-band law) and `flow_style_b_probe`'s
   H section's "hinges must be 0" checks stay valid.
6. **O-cone-6** docs: §3b.6 of the design doc, §12.9's successor here, the
   mirror's README/TOUCHPOINTS canary list (`.rw-fin` disappears → the
   extractor's canaries must move to `.rw-orbit .rw-branch .rw3-sheen ...`).

### 13.6 As built (v0.5.70)

All seven decisions shipped; the user approved them in one go ("你的建议我认为
都是对的，这七条开工"). Client + stylesheet only — **the server is untouched**:
`fin`/`hinge_x`/`seqs`/`orbit.{step_deg,arc_deg}` were already everything the
cone needs, because the slope is pixels.

**What is on screen now.** Each branch is a zero-size `.rw-branch` container
sitting exactly on its parent's bead, holding one straight `.rw-br-line` bar
(`width: hypot(dx·cell, n·q)`, `rotate(-atan2(n·q, dx·cell))`) and the beads
that branch owns, each one column right and one `q` out
(`top: -out * q`). `q = (axis - margin - band/2) / longest` is written by
`sync_scene_metrics` in **px**, next to `--cell` — both because `atan2()` and
`hypot()` are usable in this Chromium but **not with container units in
them** (a `cqw` inside `atan2()` makes the whole `transform` compute to
`none`; measured, cost an hour). The trunk is bit-identical to v0.5.69, the
park/queue/`--phase`/`--align` mechanics are the v0.5.68 ones, `.rw3-sheen`
is kept, and `.rw-fin`, `.rw3-elbow`, `.rw3-hinge` are **deleted** — the
ribbon and the elbow bars are gone from the DOM, the CSS and the extractor's
canaries (which now assert their *absence*).

**A fork off a branch nests.** When a branch's parent is itself on a branch,
its container is rendered *inside* the parent's, at `top: -po * q` — so its
spine starts on the parent's bead and it fans relative to the parent
(`--eff: slot · step`, no second phase). Depth is unbounded.

**Three engine traps, all measured, all now load-bearing:**

1. `opacity < 1` on an ancestor makes it a **grouping element**, which
   flattens 3D children — a bead's own `opacity` turned its 13px dot into
   7.85×3.05 and moved its centre ~35px off the ray. The park fade is
   therefore a **value** (`--fade`) read by the leaves (`.rw3-dot`,
   `.rw3-round`, `.rw-br-line`), never the `opacity` property on a container.
2. A bead must counter-rotate by its plane's **absolute** angle, and CSS
   cannot add an ancestor's variable to its own (a self-reference is a
   cycle). The render writes the chain on every container instead: `--sroot`
   (the top trunk-parented ancestor's slot) and `--sum` (the static, clamped
   part of the nesting chain). With `--a` alone a nested bead over-rotated by
   its parent's turn — same squash, same 35px.
3. The flat projection scales the *container*, which also scales the bead's
   content; a nested bead therefore needs the chain's **product**. Same fix,
   same shape: `--kup`/`--kown` (static, clamped per level) beside the
   dynamic `--kroot`, so `1 / (kroot · kup · kown)` is exact at any depth.
   (`--aroot` is the *clamped* root angle — §12.9's refinement, again.)

**One honest limitation.** The park push is part of the container's
transform, so a *parked* branch's origin travels with its plane and its
spine no longer lands exactly on the parent's bead in projection (measured
drift: 225px at slot 3, 494px at slot 6). The plane, the spine and the beads
move *together* — nothing inside a branch is inconsistent — and the queue is
faint and far, but `orbit_probe`'s L4a checks the anchor strictly only where
nothing is parked and reports the drift otherwise. Fixing it would mean
splitting the push out of the plane, which costs the one-transform
per-container simplicity this design is built on.

**The four probes, re-run green:**

| probe | before | v0.5.70 |
| --- | --- | --- |
| `orbit_probe.py` (own fixtures, L1-L12) | 34 checks (the ring) | **40 passed / 0 failed** (rewritten for the cone) |
| `flow_style_b_probe.py 8480` (A-I) | 66 / 66 | **66 passed / 0 failed** (C/H moved to the cone's vocabulary) |
| `flow_check.py 8480` | 8 / 0 | **8 sessions, 0 problems** |
| `rewind_probe.py` | 140 | **PASS (140 checks)** |

Two probe bugs were fixed on the way and are worth the note, because both
looked like product bugs first: the geometry model's `rot_x` dropped `z`
(harmless for one rotation, wrong the moment a container is nested — it
reported a nested bead 36px off the law when the paint was exact), and
`flow_style_b_probe`'s K1 hard-coded the longest *live* session at 117
rounds (it is 118 now, so it reads the length from the API like the rest).
`cargo test -p rushi-web` stays at 71; the wasm check and `trunk build` are
clean.

### 13.7 The three defects live use found (2026-10-04, v0.5.71)

The user reported three bugs right after v0.5.70 shipped, and asked for the
*causes* first ("详细调研这几个bug的原因"). All three were reproduced and
measured before any code changed.

**Bug 1 — "why does a branch only turn above its parent?"** Because it did:
`--a: clamp(-arc, eff, +arc)` with `arc = 60`. Sweeping `--rw-scroll` through
more than a full turn and reading the *rendered* transform (the `calc()` vars
cannot be parsed off `getComputedStyle`; `matrix3d`'s m22/m23 give the angle)
showed the angle never leaving {±60}: 0, 60, 60 … The far bead's `dy` from its
parent was **always negative** (above), e.g. -210px at rest for the longest
branch. `SCENE_AXIS_FRAC = 0.62` — a v0.5.68 leftover whose own comment said
"the bounded arc lives on the upper half" — was the structural half of it.

**Bug 2 — "it detaches from the parent and then shrinks out of sight."**
Past the arc, `--over` grows without bound (0 → 300 over the sweep) and drives
two things: the container's `translate3d(0, 0, over·-22px)`, which moves the
container's **origin** off its parent's bead (measured drift 0 → **86.9px**),
and `--fade: clamp(0.08, 1 - over/70, 0.92)` on the leaves, while the
perspective shrank the dots from 13px to **2.2px**. A park that cannot end.

**Bug 3 — "entering the flow view shows the previous version's purely
horizontal layout."** The scene mounted with **no inline geometry at all**:
`--rw-scroll`/`--rw0`/`--axis`/`--r`/`--q`/`--cellpx`/`--dpp` all empty,
`--cell` still the raw `clamp(26px, calc(100cqw / N - 4px), 64px)`, `--phase`
still an unresolved `calc()`. `sync_scene_metrics` has exactly four callers —
the flow effect (deps `view`/`tree`/`selected`), its `Timeout(0)`, the window
`resize` listener and `on_down` — and **entering the History changes none of
them**: the tree had been fetched while the split plugin body was showing (so
the effect early-returned with no `#rw-flow-scroll` in the DOM), and opening
the History only re-rendered chrome. With `--q` empty, `var(--q, 0px)` is 0
(beads sit on the trunk) and the spine's `atan2()` eats an empty `--cellpx`
(container unit → invalid → `transform: none`), which is precisely a straight
horizontal line. The scene painted that way until *any* pointer press
(`on_down` measures) or a tree refetch fixed it — "click the card and it
appears".

**The decisions (D-cone-8..12 — all approved by the user, 2026-10-04).**

| # | decision | supersedes |
| --- | --- | --- |
| D-cone-8 | The angle becomes a **full circle**: `mod(eff + 180, 360) - 180`. The park (`--over`, the z push, the park fade) and the queue are **deleted**; every branch is always on the cone, 30° apart. | D-orb-2, D-orb-6 |
| D-cone-9 | The auto-fit fits the **smaller half**: the axis moves to `0.5·h`, `r = min(axis, h-axis) - margin - band/2`, `q = r / longest`. Branches get ~30% shorter (measured on the live `rewind` session: q 30.0 → 23.3px, r 210 → 163px) — the user accepted the shorter branches. | D-orb-1's fit |
| D-cone-10 | The park fade is replaced by a **depth dim**: the half pointing away from the viewer reads `1 - 0.35·max(0, sin a)` (near 1.0, far 0.65); straight down stays lit. Still a **value** on the leaves, never the `opacity` property on a container. | D-orb-6 |
| D-cone-11 | The one-branch `solo` swing is **gone**: a lone branch orbits the trunk like every other one, and the `.rw-orbit.solo` class is no longer rendered. | D-orb-9 |
| D-cone-12 | Bug 3's mechanism: the scene **arms its own probe** — a per-mount `ResizeObserver` on `#rw-flow-scroll` (it fires once when observed and on every box change: mount, split↔full, sidebar, panel) plus the layout signal in the flow effect's deps as a second line of defence. | — |

**Engine finding, measured before the fix was written:** CSS `mod()` and
`rem()` **work** in the target Chromium (131.0.0.0), including *negative*
arguments and through a custom-property chain —
`mod(calc(var(--phase) + 180), 360) - 180` computes exactly (`mod(-390, 360)
= -30`). So the whole wrap stays in CSS and a scroll frame still writes exactly
one value. A live CDP override of that one transform (`park dropped`) was used
as the fix *preview*: angles then span the whole circle, `tz` stays 0, the far
end goes below the trunk — and with the old slope the down-half overflowed the
panel by ~65px, which is what made D-cone-9 a decision rather than a detail.

### 13.8 As built (v0.5.71)

Client + stylesheet only again (**the server is untouched**; `arc_deg` is still
emitted and simply not read any more — `--arc` is no longer written on the
track).

* `style.css`, `.rw-branch`: `--a: calc(mod(calc(var(--eff) + 180), 360) -
  180)`, `--aroot` the same wrap, `transform: rotateX(calc(var(--a) * 1deg))`
  (the `translate3d` push is gone), `--fade: calc(1 - 0.35 * max(0,
  sin(calc(var(--a) * 1deg))))`, `--over` deleted, `.rw-orbit.solo` deleted,
  and the two `--over`-based `--fade` overrides (flat + reduced motion)
  deleted with it. The depth dim reads `--fade` on the leaf elements, and the
  per-state factors are now *multiplied* into it
  (`.off .rw3-dot` × .6, `.abandoned .rw3-dot` × .45, `.rw3-round` × .6) —
  without that, most of a branch's rounds are `off` and the dim would never be
  visible on them.
* `rewind.rs`: `SCENE_AXIS_FRAC = 0.5` with `room = axis.min(h - axis)`;
  `arm_scene_probe()` / `arm_scene_probe_now()` (one `ResizeObserver`, re-armed
  by identity when the scene re-renders its node, `disconnect()` on
  replacement); an `Effect::new` *inside* `flow_scene` that calls
  `arm_scene_probe_now()` (with one `Timeout(0)` retry while the tree is still
  loading); `layout_mode` added to the flow effect's deps; `solo` dropped from
  the render; the per-level `clamp(±arc)` removed from the static nesting
  chain (`--kown`, `--sum`) — on a cone ±120° is a place like any other, and
  the old clamp parked two children on top of each other.
* `Cargo.toml`: the `ResizeObserver` web-sys feature.

**The four probes, re-run green after the change:**

| probe | v0.5.70 | v0.5.71 |
| --- | --- | --- |
| `cargo test -p rushi-web` | 71 | **71 passed** |
| wasm `cargo check` + `trunk build` | clean (13 pre-existing warnings) | clean |
| `orbit_probe.py` (own fixtures, L1-L12) | 40 / 0 | **44 passed / 0 failed** (the arc/queue checks became the full-circle ones: no park, `|angle|` reaches 180, a whole turn spent on the **wheel** returns every bead, the depth dim is exactly `1 - 0.35 sin a`, and the lone branch goes above *and* below) |
| `flow_style_b_probe.py 8480` | 66 / 0 | **66 passed / 0 failed** (H re-worded; `solo` is now an absence canary) |
| `flow_check.py 8480` | 8 / 0 | **8 sessions, 0 problems** |
| `rewind_probe.py` | 140 | **PASS (140 checks)** |

Three probe lessons worth keeping:

1. The geometry model's phase must be read from **`--rw-scroll`**, not from
   `scrollLeft`: since v0.5.69 that proxy is `scrollLeft + turn`, and a wheel
   gesture keeps turning the cone after the track's own scroll range runs out.
   With a wheel-driven check the old model reported a 278px "law" error that
   was entirely the probe's.
2. A wheel delta must be an **integer** if the check needs an exact sum
   (`scrollLeft` is an integer, so fractional deltas leak to rounding — 16
   × 79.69px came out 11px short).
3. A full circle means the near and the far half of the cone share a ray, so
   a bead *behind* another is legitimately covered. The hit test now accepts a
   cover only when the two dots actually overlap (≤12px apart); anything else
   covering a bead is still a failure.

## 13.9 Even distribution of a fan (proposed 2026-10-04, v0.5.72)

**The ask (user, verbatim):** "分支分布按 360 度平均分布，例如有三条分支，那么分支
之间的角度为 360/3 = 120 度，平均分布会让分支不拥挤，方便用户点击" — distribute the
branches of one fan evenly over the full circle, so the angles are `360/k`
instead of the fixed 30° step.

### Why: the fixed 30° step is crowded (measured)

The step is a **constant** (`ORBIT_STEP_DEG = 30`, the server's `orbit.step_deg`)
while the fan size is not, so a fan of three branches occupies only 60° of the
circle and lands in the upper half. Measured on the live `rewind` session
(4 fins: a **root fan of 3** + one nested fan of 1 — so the user's example of
"three branches" is exactly this fixture; every other live session has 0 or 1
fin), by re-writing `--step` on the painted scene and reading the beads'
`getBoundingClientRect()`:

| layout | angles | projected rays | bead pairs < 13px apart | branch beads covering a **trunk** bead |
| --- | --- | --- | --- | --- |
| now: step 30, server slots (0, 2, 3) | 0°, 60°, 90° | 272 / 0 / 208 px | **8** | **7** |
| step 30, slots re-based per fan (0, 1, 2) | 0°, 30°, 60° | 272 / 0 / 221 px | 3 | 1 |
| **even 360/3 = 120°, slots re-based** | 0°, 120°, −120° | 272 / 0 / 258 px | **2** | **1** |
| (hypothetical N=4) step 90 | 0°, 90°, 180° | 272 / 0 / 272 px | 1 | 1 — but see the ±90° note |
| (hypothetical N=5) step 72 | 0°, 72°, 144° | 272 / 0 / 243 px | 1 | 1 |

Two things this measurement settles:

1. **The angle a fan gets must be re-based per fan.** `fin` is a *global*
   index over every branch of the session, so the root fan of the live fixture
   holds slots `{0, 2, 3}` (slot 1 belongs to the nested fan). Driving
   `360/3 = 120°` off those numbers put **two branches on exactly the same
   angle — 0.0px apart, 7 overlapping bead pairs** — because slot 3 wraps to
   0. Each fan therefore needs its **own** ranks `0..k-1`, with the global
   `fin` kept only as an identity (`data-fin`).
2. Even splitting removes the crowding that the user reported: branch beads
   sitting on top of trunk beads go **7 → 1**, and overlapping bead pairs
   **8 → 2**.

### The one geometric caveat: ±90° is edge-on

A branch's screen offset is `−q·cos a` (recall `rotateX` maps its ray to
`(0, −h·cos a, −h·sin a)`), so at `a = ±90°` the ray loses **all** of its
projected length: it points at, or away from, the camera. Measured at the
N=4 spacing: the edge-on branch's bead landed **1.7px from a trunk bead** — it
is drawn as a full-size dot sitting on the trunk row, indistinguishable from a
trunk node. Note this is *not* "invisible": the bead is still there, still
counter-rotated, still clickable (and a click aligns the fan, which swings it
to the front) — but it reads as a trunk bead.

Two facts make this the only singular case:

* the split is even and 0-based, so a branch lands on ±90° exactly when
  `k ≡ 0 (mod 4)` (`k = 4, 8, ...`); for `k = 3, 5, 6, 7` the closest branch is
  30°, 18°, 0°, ~13° away from it;
* two branches at `±a` share a screen **height** (`cos` is even) and differ only
  in depth — the mirror pair D-cone-10 already dims one of them
  (`1 − 0.35 sin a` ⇒ 0.65 far / 1.0 near) and the perspective separates them
  slightly. This is inherent to a full circle, not to this change.

### The plan

**D-fan-1 (the step).** `step(fan) = 360° / k` where `k` is the number of
branches **in that fan**: the root fan counts the trunk-parented branches, a
nested fan counts the children of that branch. Nothing server-side: the client
already builds the forest (`roots` + `kids`), so the whole change is
client + stylesheet.

**D-fan-2 (per-fan ranks).** A container's `--slot` becomes its **rank inside
its own fan** (`0..k-1`, in the server's `fin` order so it stays stable across
reloads); `data-fin` keeps the global index. The root fan's `--align` becomes a
*rank* too (it already is the topmost ancestor's fin — same value only while
the fins are dense).

**D-fan-3 (the aligned branch stays at the front).** For the root fan the
`--align` rank is still subtracted *before* the step, so at rest the selected
(or current) branch points straight up (D-orb-7 kept: "the alignment rule
always wins at rest"). Sub-decision on the `k ≡ 0 (mod 4)` singular:
* **(A) keep it** — a branch exactly at ±90° when `k` is a multiple of 4; it is
  always one click (or one scroll) away from the front. Recommended while no
  live session has 4+ root branches.
* **(B) half-step offset** for **even** `k` (`+180/k`): no branch is ever
  edge-on, the fan is symmetric about the vertical, but no branch points
  exactly up (`k=4`: ±45°/±135°; `k=2`: ±90° — still singular, so B would need
  the cap below).
* **(C) hybrid** — offset only when a branch would land within ±5° of ±90°
  (`k ≡ 0 (mod 4)`): exact for `k = 1, 3, 5, 6, 7`, safe for `k = 4, 8`.
  **Recommendation: (C)**, i.e. (A) everywhere it can be exact and (B) only
  where the circle would put a branch on the camera's axis.

**D-fan-4 (nested fans).** The same even split, but the fan is **centred on
180°** (opposite the parent's own ray) and its spacing is **capped at 120°**:
`spacing = min(360/k, 120)`, angles `180 + (j − (k−1)/2)·spacing`. This keeps
D-cone-7's measured rule for the cases that matter — a lone child sits at
**180°** (mirrored below its parent, never on the parent's own continuation
ray, which is what made a +30° child hide 4.5px behind the parent's next bead)
— and sends two children to 120°/240° instead of the singular 90°/270°.

**D-fan-5 (the rotation rate).** Unchanged (`ORBIT_DEG_PER_WIDTH = 240`, i.e.
`--deg-per-width`): a bigger step only means fewer branch-steps per panel
width (`240k/360`), and the rate is a feel parameter, not a layout law. (If the
user wants "one panel width = one whole turn", that is a **one-number** change;
not proposed, since it would make a 3-fan feel three times slower.)

**D-fan-6 (no fit change).** The auto-fit already spends the *smaller* half of
the panel (D-cone-9, axis at `0.5·h`) and a branch's screen offset is
`|cos a| ≤ 1`, so a fan spread over the full circle still fits — the worst case
(0°/180°, straight up and straight down) is exactly what v0.5.71 already sized.

### Implementation (client + stylesheet, no server change)

The step can no longer be one number on the track, so the render writes the
**degrees** it already computes instead of the raw slots:

* `rewind.rs` (`Cone`): carry a per-fan `(step, offset)` and the fan's rank
  instead of the scene-wide `self.step`; write `--th` (this container's angle
  inside its parent's plane, in degrees: the root fan `(rank − align)·step`,
  a nested fan `(j − (k−1)/2)·spacing + 180`), `--rdeg` (the **topmost
  ancestor's** aligned angle, the same for the whole sub-tree, which is what
  `--root_a` needs — today `(sroot − align)·step` is re-derived in CSS, which
  cannot work once the step differs per container), `--sum` (the static nesting
  chain, now a float) and `--kup`/`--kown` (floats, from `flat_k` at the same
  degrees). `--slot` stays (rank, for debugging and for the probes), `data-fin`
  stays (identity).
* `style.css`: `--root_a: calc(var(--rdeg, 0) + var(--phase, 0))`,
  `--theta: var(--th, 0)`; the nested rule keeps `--eff: var(--theta)` (the
  parent's turn is already in force — no double phase). `--step`/`--align`/
  `--sroot` then leave the stylesheet entirely; the canary list of the mirror's
  extractor is unchanged (no selector appears or disappears).
* `flat_k`, `sum`, `kown` become `f64` end to end (a 7-fan has a 51.43° step).

### Verification

* `cargo test -p rushi-web` — layout tests are angle-agnostic; add one assertion
  that a fan of `k` gets `360/k`.
* `e2e/orbit_probe.py` — the law check changes from `(slot − align)·step` to
  `(rank − align)·360/k`; add: the live fixture's root fan is exactly
  0°/120°/240°, `Time inject`'s lone branch stays at 0°, the lone nested child
  is at 180°, and every fan's angular gaps are equal.
* `e2e/flow_style_b_probe.py` (H) — read `--th`/`--rdeg` instead of
  `--slot`/`--step`; keep the "no bead leaves the panel" and hit-test checks.
* `e2e/flow_check.py` (HTTP only) and `e2e/rewind_probe.py` (Style A) are
  unaffected.
* The measurement harness for the table above lives in `/tmp/step_probe.py`
  (throwaway).

### Risks / open items

* The mirror-pair proximity (two branches at `±a` are at the same screen height)
  is inherent; the depth dim D-cone-10 is the mitigation. If the user wants the
  *far* branch to move, that is a new decision, not part of this change.
* `animation-timeline` is unused in the scene (the phase is a var), so nothing
  here touches D9.

### Open questions for the user

1. **D-fan-3**: (A) exact "aligned branch straight up" always, accepting
   ±90° for `k ≡ 0 (mod 4)`; or (C) the hybrid (offset only in that case)?
2. **D-fan-4**: is "a nested fan is centred *below* its parent" right, or should
   a nested fan start at the parent's own ray (+0°) when `k ≥ 2`?
3. **D-fan-5**: keep the rotation rate at 240°/panel width?

## 13.10 As built (v0.5.72) — the even fan

**The user's decisions (2026-10-04, all three answered):** D-fan-3 → **(C) the
hybrid** ("按你建议来"): the aligned branch points straight up, and a fan whose
size would land a branch exactly on ±90 (`k % 4 == 0`) is nudged half a step
instead. D-fan-4 → **centred on 180** ("嵌套扇以 180° 为中心"). D-fan-5 → the
rotation rate is **unchanged** ("旋转速率暂时不变"). D-fan-1/2/6 were not in
question (the step is `360/k` per fan, ranks per fan, no fit change).

**As shipped.** Client + stylesheet only; the server is untouched (its
`orbit.step_deg`/`orbit.arc_deg` are now *both* unread — the design doc says
so, so nobody trusts them).

* `web-leptos/src/rewind.rs`:
  * `FAN_EDGE_ON = 0.087` (5° off the camera's axis), `fan_step(k, nested)`
    (`360/k`, capped at 120 for a nested fan), `fan_root_theta(rank, align, k)`
    (the signed step distance plus the half-step nudge when `k % 4 == 0`) and
    `fan_nested_theta(rank, k)` (centred on 180, quarter-step nudge when
    180 ± 90 would reappear).
  * `Cone` loses its scene-wide `step`; `branch()` takes `rank`/`step`/`theta`/
    `rdeg`/`sum` (all the angles in degrees, `f64`) and writes `--slot`
    (rank), `--step` (the fan's spacing, informational), `--th`, `--rdeg`,
    `--sum`, `--kup`, `--kown`.
  * The child fan is the parent's `kids` list enumerated (rank), not the old
    ±2-slot pattern; `align` becomes a **rank** in the root fan; the track's
    inline style is down to `--cols`/`--lanes`.
  * A `#[cfg(test)] mod fan_angles_tests` writes the law down as assertions
    (even gaps, never edge-on, the front's nudge rule, a nested fan below its
    parent). The wasm crate cannot run host tests, so these are *compiled* for
    `wasm32-unknown-unknown` (`cargo test -p rushi-web-ui --no-run --target
    wasm32-unknown-unknown` ✓) and *executed* by extracting them verbatim into
    a standalone host harness (`/tmp/fan_check.rs`, `rustc --test` ⇒ **2
    passed**), plus by the browser probes below.
* `web-leptos/style.css`: `--root_a: calc(var(--rdeg, 0) + var(--phase, 0))`,
  `--theta: var(--th, 0)`; the nested rule keeps `--eff: var(--theta)` and no
  longer overrides `--theta`; `--step`/`--align`/`--sroot` leave the stylesheet.
  No selector appears or disappears, so the mirror's canary list is unchanged.

**Measured on the live `rewind` session** (the user's own example: a root fan of
3 + one nested child), by re-reading the painted beads' rects — before (fixed
30° step, server slots) vs after:

| | angles | branch beads covering a **trunk** bead | bead pairs < 13px |
| --- | --- | --- | --- |
| before | 0° / 60° / 90° (+ nested at −60°) | **7** | **8** |
| after | 0° / 120° / −120° (+ nested at **180°**) | **1** | **2** |

**The probes, all re-run after the change:**

| probe | v0.5.71 | v0.5.72 |
| --- | --- | --- |
| `cargo test -p rushi-web` | 71 | **71 passed** (the server is untouched) |
| `cargo test -p rushi-web-ui --no-run --target wasm32` | — | compiles (13 pre-existing warnings); the fan-law assertions run in a host harness: **2 passed** |
| wasm `cargo check` + `trunk build` | clean | clean (13 pre-existing warnings) |
| `orbit_probe.py` | 44 / 0 | **48 passed / 0 failed** (L2h-L2k added; L7a now proves the alignment through `--rdeg`) |
| `flow_style_b_probe.py 8480` | 66 / 0 | **68 passed / 0 failed** (H11/H12 added; H reads `--th`/`--rdeg`) |
| `flow_check.py 8480` | 8 / 0 | **8 sessions, 0 problems** (HTTP only) |
| `rewind_probe.py` | 140 | **PASS (140 checks)** |

**Two trap notes worth keeping.** (1) A fan's angle must be built from a
**per-fan rank**: driving `360/3` off the root fan's *global* slots `{0, 2, 3}`
put two branches on the same angle (0.0px apart, 7 overlapping pairs) because
slot 3 wraps to 0. (2) A fan of one has no "gap" — its spacing is the whole
circle, so a check that compares its single angle's gap to a *capped* spacing
fails by construction (the nested fan's cap is 120, its lone angle 180).

## 14. Round 5 — the relaxation model and the tilted cone (proposed 2026-10-04)

**The two asks (user, verbatim):**

1. *"你的C方案可以，我还有在你的C方案上更好的解决方法：为节点和连线创建实体，实体间存在
   斥力，会自动排斥开，同时可以避免节点重合和连线干扰的问题"* — model the nodes **and the
   connectors** as bodies with mutual repulsion, so that overlaps and line interference
   resolve themselves.
2. *"圆锥面的开口方向不要正对着水平右方向，而是向右上一点角度。这样在旋转时，在主分支上方的
   分支会因为圆锥面这样的角度被放大……上方放大的这个分支保持和主线节点相同的大小"* — tilt
   the cone so its opening points up-right instead of straight right; the branch above the
   main line then reads as the near/focused one, but it must keep the **main line's node
   size**.

This section is the research + plan that follows. Section 14.8 lists the decisions I need.

---

### 14.1 What the scene is today (measured, not remembered)

**The mechanism.** The server projection gives every round a column `x`, a lane and a branch
membership (`fin`). The client turns that into, per branch: a container sitting on its parent's
bead, `--th` (the fan angle, degrees), `--n` rounds, `--dx` columns; per bead: `--x` and
`--out` (1-based steps out); and one spine bar per lit run. The stylesheet does the rest:

```css
.rw-branch   { top: var(--axis); transform: rotateX(calc(var(--a) * 1deg)); }
.rw-branch .rw-branch { top: calc(-1 * var(--po) * var(--q)); }        /* nested */
.rw-branch .rw3-node  { top: calc(-1 * var(--out) * var(--q)); }        /* a bead */
.rw-br-line  { width: hypot(dx*cell, n*q); transform: rotate(-atan2(n*q, dx*cell)); }
```

So one step out is `(cell, −q·cos a, −q·sin a)` in CSS axes (x right, **y down**, z toward the
viewer): **`cos a` sets the height and `sin a` sets the depth**, and the two are orthogonal
coordinates of the same circle. `#rw-flow-track { perspective: 1400px }` magnifies by
`k = 1400/(1400 − z)` — so the near half of the circle is **bigger than 1.0** and the far half
smaller.

**The rotation.** `--phase = (scrollLeft − --rw0) · --dpp`, `--dpp = 240°/panelWidth`, and a
branch's effective angle is `θ + phase`. Every branch therefore turns by the same angle as the
user scrolls: **the layout must be judged over the whole circle, not at one screenshot.**

**The live fixture (`rewind`, 2026-10-04).** `cols 34`, `cell 32.359px`, `q 25.714px`,
`axis 209px` (panel height 418px), `d 1400px`, `r = 180px`; 4 fins — a **root fan of 3**
(0°, 120°, −120°) plus one **nested** child (180°, `po = 1`).

**The phase sweep.** Swept **on the live page** (CDP wheel events, 24 phases at 15°, the bead
centres read off the painted dots) and cross-checked with an analytic model of the same laws:

| quantity | value (measured live) |
| --- | --- |
| phases with **no** bead pair < 14px | **0 / 24** |
| pairs < 14px per phase | **2 … 6** (the best phases are 2, the worst 6) |
| the nested `fin1 r21` × `trunk r31` pair | **0.0px at *every* phase** (24 / 24) |
| the next-closest pairs | the mirror pair `fin2 r22` × `fin3 r23` (6.1-12.6px) and a branch bead against a trunk bead (1.1-3.6px at the edge-on phases) |
| phases where some branch is within 8.6° of edge-on (analytic model, 2° steps) | 54 / 180 |

The first version of this section claimed the exact coincidence only happened in 15 of 180 phases
— that was **wrong**, and the bug is worth writing down: the model did not rotate a *nested*
container's offset by its parent's turn, so the child only met the trunk near phase 0. Measured
on the page, the nested coincidence is **phase-independent** (see K1 below).

**Three classes, three different causes.**

* **K1 — the exact coincidence, and it is phase-independent.** A nested branch whose container
  offset `po` equals a bead's `out` (a child of its parent's **first** bead — the live case,
  `po = out = 1`) has its first bead exactly on a trunk bead: the two radial contributions
  cancel (`(po − out)·q = 0`) *and* the depth cancels too (`−(po − out)·q·sin a = 0`), so the
  perspective cannot separate them either. Measured on the page at all 24 sampled phases:
  **0.0px every time**; `elementFromPoint` there returns the **branch** bead, so the trunk round
  is permanently covered. Any angle- or phase-based fix is therefore useless against K1 — only
  moving the child itself (§14.3) can clear it.
* **K2 — the edge-on pass.** At `a = ±90°` the vertical offset is *identically* zero: the
  branch's beads lie on the trunk row, one column apart, each a few px from the trunk's own
  beads (the perspective separates them a little: measured live, 1.1-3.6px at the edge-on
  phases). **No angular or radial
  change can remove K2** — it is the geometry of a full circle (D-cone-8) plus a trunk bead in
  every column plus "one step out = one column right". It is a *transient* (≈ ±8.6° of phase,
  ≈ ±37px of scroll) but the user may stop anywhere.
* **K3 — the mirror pair.** Two branches at `±a` share a height (`cos` is even) and differ only
  in depth, so only the dim (D-cone-10) and the perspective's few px separate them: 4.2px at
  the live rest phase. Inherent to a symmetric fan.

**The size measurements (the motivation for ask 2).** Painted dot widths at rest:

| bead | angle | z | dot |
| --- | --- | --- | --- |
| trunk (any) | — | 0 | **13.00 x 13.00** |
| fin 0, round 14 (aligned) | 0° | 0 | 13.00 |
| fin 1, round 21 (nested) | 180° | 0 | 13.00 |
| fin 2, round 22 (far) | +120° | −22.3 · out | 12.80 |
| fin 3, round 29 (near) | −120° | +22.3 · out | **14.63** (+12.6%) |

fin 3's *last* bead grows to 14.63px: `k = 1400/(1400 − 7·25.714·0.866) = 1.125` ⇒
`13 · 1.125 = 14.63` — the model and the page agree, so **the near branch is up to 12.6% bigger
than the main line today**. That is exactly what the user is describing.

**The bar the suite currently accepts.** `orbit_probe.py`'s `L8b` is literally *"every visible
bead is clickable, **or covered by a bead on the same px**"* — it tolerates K1. Any new model
has to raise that bar.

---

### 14.2 The constraints any solution must respect

| # | constraint | where it comes from |
| --- | --- | --- |
| C1 | The trunk is the timeline: its beads never move off their column or off the axis row. | D1 / "the trunk stays a straight line" |
| C2 | A branch is **one straight ray** from its parent's bead. A connector therefore cannot be an independent body — it moves **with** its branch. A bead may slide **along** its own ray (that keeps it on the ray and the spine straight). | D-cone-6, "every connector must be a straight line with no bends" |
| C3 | The rotation stays CSS + scroll driven. **No per-frame simulation**: the solver runs once per layout and its result is baked into the same custom properties. | D9/D7, the wheel/scroll design |
| C4 | The objective must be **phase-independent** — the user can stop at any phase. | `--phase` = scroll |
| C5 | Deterministic: same data + same panel ⇒ the same picture (no RNG, no time), so the probes can assert it. | the probe suite |

C2 is the one place where the literal form of ask 1 has to bend: a connector that floats free
would have to bend or detach from its parent, and both are locked against. The physical reading
that *is* legal: **a branch is a rigid body** (its hinge is its parent's bead; it may turn and
it may start a little farther out) and **a bead may slide along its own branch**.

---

### 14.3 Proposal A — "entities and repulsion" as a layout-time relaxation

**A1 — the bodies and the legal degrees of freedom.**

| body | may move | may not |
| --- | --- | --- |
| trunk bead | — (fixed) | its column, its row |
| branch | its angle θ within `±CAP` of its fan's even angle; its radial offset `--d0 ∈ [0, 1]` steps (all its beads shift out together; the spine already has `--d0`) | its hinge (= its parent's bead), its straightness, its rounds' order |
| branch bead | radially along its own ray, if we also let the spine take `--d1` (the last bead's step) — the ray stays straight either way | its ray, its column ordering |
| spine | nothing on its own; it is redrawn from the first to the last bead | leaving its branch |

**A2 — the objective.** Minimise the worst violation over a phase set Φ (72 phases at 5°; the diagnosis
swept 180 at 2° and saw no phase pass), with a hard bar:

* **R-a** no two beads within 1px at any phase (this kills K1 outright);
* **R-b** at the *rest* phase, no two beads within one dot (13px);
* **R-c** over the whole rotation, a pair < 13px is allowed **only** when one of the two is on a
  branch within ±8.6° of edge-on at that phase (K2, admitted);
* **R-d** no bead within 6px of a *foreign* branch's spine at any phase;
* **R-e** the fan still reads even: a branch's angle stays within `±CAP` of `fan_root_theta` /
  `fan_nested_theta` (see D-phys-6).

**A3 — the solver.** Seed = today's even fan. Per iteration: evaluate the violations over Φ
analytically (the client has `q`, `cell`, `axis` in px after `sync_scene_metrics`), then apply a
damped correction per branch (angle) and per branch (radial), accumulate, clamp to the legal
range, and repeat (≤ 40 iterations). Complexity is tiny: |Φ| × pairs ≈ 72 × (48²/2) ≈ 83k
distance checks per iteration ⇒ a few ms in wasm, and only when the data or the panel changes.
The result is memoised on `(projection hash, panel w/h, cell, q)` so a re-render cannot move the
picture (C5) and there is no render loop.

**A4 — what it fixes, and what it cannot.**

* **K1 → gone, guaranteed** (R-a). This is the defect the user actually hit.
* **K3 → much better**: a repulsion between the two branches of a mirror pair pushes them *apart*
  symmetrically, so the 4.2px gap grows to a dot's width (if `CAP` allows the turn).
* **K2 → not fixable by any angle or radius**, see §14.1. Three ways out (D-phys-5):
  * **(a) accept + cue**: when `|cos a| < 0.15` the whole branch is *near edge-on* — dim it (and
    optionally shrink its dots) so it reads as "this branch points at you" instead of as trunk
    beads. Cheap, honest, does not fix the overlap.
  * **(b) hard fix**: give branch beads a **half-column x offset** (they then never share a
    column with a trunk bead ⇒ ≥ 16px = half a cell apart even at edge-on). Real fix, at the
    price of the branch's rounds sitting half a column off the trunk's grid.
  * **(c) accept silently** (documented).

**A5 — where it lives.** Client-side, as a pure function (the same pattern as `fan_angles_tests`:
written as testable Rust, compiled for wasm, and run in a host harness by extraction + asserted
by the browser probes). The server keeps its projection unchanged — it does not know the px
metrics, and making it viewport-dependent would poison the cache key.

---

### 14.4 Proposal B — the tilted cone and the size cap

**B1 — the size cap (the part that is unambiguous).** Cap the perspective magnification at the
main line's size, per bead, about the bead's own centre:

```
scale = min(1, 1/k) = min(1, (d − z)/d) = min(1, 1 + z/d)
```

* z ≤ 0 (away) ⇒ the bead keeps its shrunken size: the far half still reads as background.
* z > 0 (toward the viewer) ⇒ the bead is scaled *back* to 13.00px, so the focused branch is
  exactly the main line's size — the user's ask.
* Scaling about the bead's **centre** leaves the centre where it is, so the spine (drawn
  separately, from the first to the last bead) stays exactly on the beads. No locked rule moves.

CSS cannot know `z` on its own (it accumulates along a nesting chain), so the **render writes it
as a number of steps** (`--zp = Σ rᵢ·sin aᵢ`) and the **layout writes `--qk = q / 1400`** as a
plain number ⇒ `scale = min(1, 1 + var(--zp) * var(--qk))` — no length division in CSS.

**B2 — the tilt, and one uncomfortable fact.** *Up* (the screen height, `cos a`) and *near*
(the depth, `sin a`) are **orthogonal directions of the same circle**. So "the branch above the
main line" is *not* automatically the near one: at any phase the upper region holds a
mirror pair, one near (magnified) and one far (dimmed). Two candidate readings of the ask:

* **(i) roll the fan's frame about the view axis** — `transform: rotateZ(ψ) rotateX(a)` on each
  branch container (ψ ≈ −8°…−12°). This *is* "the cone's opening points up-right": the cone's
  axis is the column direction, and the roll gives it an upward component, so the whole fan
  leans up-right and its beads drift upward as they go out — **while the trunk stays exactly
  horizontal**. It does **not** make "up" equal "near" (nothing can), and the mirror pair still
  shares a height; the depth dim + the size cap remain the separators.
* **(ii) bias the fan's reference direction** (a constant added to the phase/θ, e.g. −25°): the
  focused/aligned branch would rest at *up-and-near* (magnified, then capped by B1) and its
  opposite at far. One constant, but it breaks D-fan-3's "the aligned branch points straight
  up".

My recommendation is **(i)** — it is the literal reading, it keeps "aligned = straight up", and
it adds the up-right lean the user asked for — with **(ii)** available as a small constant if
they want the focused branch to rest *near* as well.

**B3 — the cost of the tilt (fit).** The fit is `r = min(axis, h − axis) − 16 − 13`,
`q = r / longest`; with the roll, the top of the content is
`longest·(q·cos ψ + cell·|sin ψ|)` — about 30.9px per step against 25.7px today (+20% at ψ = 10°),
and the axis row (currently `SCENE_AXIS_FRAC = 0.50`) must be re-centred. Both are a *little*
more branch shortening on top of D-cone-9's ~30%. ψ must be chosen by look, not by taste:
−8° is ≈ 4.5px/step of drift (barely visible), −15° is ≈ 8.4px/step (a clearly slanted cone).

**B4 — does the tilt apply to the flat (reduced-motion) projection?** It is a *static* lean, so
it can: `--kroot`'s law would need the same ψ. Decision D-slant-4.

---

### 14.5 Verification plan

New/raised checks (the existing suite counts 48 + 68 + 8 + 140 today):

| id | check |
| --- | --- |
| **R1** | at the rest phase, zero bead pairs < 14px (today: 4 — the nested 0.0px pair, the mirror pair at 6.1px, and 13.0px) |
| **R2** | swept over N phases driven by real wheel events, **zero pairs < 1px** (today: the nested `fin1 r21` × `trunk r31` pair is 0.0px at *every* phase) |
| **R3** | over the same sweep, every pair < 14px has a branch within ±8.6° of edge-on, or is the K1 pair once §14.3 has moved it (today: 2-6 pairs per phase, and the mirror pair is 6.1-12.6px apart) |
| **R4** | no bead within 6px of a foreign spine at the rest phase |
| **R5** | the solver is stable: two loads of the same session at the same panel give identical `--th`/`--d0`/`--zp` values |
| **R6** | with the cap: a bead on the near branch measures 13.00px (today 14.63px) and the far branch stays < 13.00px |
| **R7** | with the tilt: the trunk's beads are still exactly on the axis row (`|Δy| < 0.5px`) and a branch's ray is still one straight line (the spine's endpoints coincide with its first/last bead) |
| **R8** | the even fan still reads even: every angle is within `CAP` of the fan law, and a fan of k has k distinct angles |
| `L8b` | raised: "every visible bead is clickable" — a bead may only be covered by itself (K1 gone) |

The phase-sweep harness (`/tmp/sweep2.py`, a model that reproduces the painted page) is the tool
for R2/R3; the browser probes do R1/R4/R6/R7 on the real DOM.

---

### 14.6 Step plan (once the decisions land)

1. **O-relax-1** — the law as a pure function + its unit assertions (`relax_*` next to
   `fan_*`), host-harness extraction, wasm compile.
2. **O-relax-2** — the layout pass: read `q`/`cell`/`axis`, run the solver, write the result
   into a memoised resource the cone render reads; the render gains `--d0`, `--d1`, `--zp`.
3. **O-relax-3** — the K2 decision implemented (D-phys-5), the `L8b` bar raised.
4. **O-slant-1** — the cap (`--zp`, `--qk`, the per-bead `scale`) + `R6`.
5. **O-slant-2** — the tilt (D-slant-2/3/4): the stylesheet's `rotateZ`, the flat law, the fit
   and the axis re-centre.
6. **O-slant-3** — docs (§3b.6 + §6 numbers), the mirror's canary/gone-set check, probes, both
   repos, a version bump.

Everything stays client + stylesheet: **the server projection does not change** (its
`orbit.step_deg`/`arc_deg` stay unread).

---

### 14.7 Non-goals (this round)

* No live per-frame physics, no continuous animation beyond the existing scroll-driven phase.
* No change to the trunk, the columns, the states, or the click/hit-test contract.
* No zoom, no mini-map, no second view style (D4/D-orb-10 still hold).
* Connectors do not become free bodies (C2).

---

### 14.8 Open questions for the user

**A — the relaxation**

* **D-phys-1**: do you accept the *constrained* form of ask 1 — bodies are **branch** (angle +
  radial offset) and **bead** (radial slide), and a connector moves with its branch (it can
  never float free)? **[recommended: yes]**
* **D-phys-2**: the solver runs **once per layout** (client side, deterministic, memoised) and
  its result is baked into CSS custom properties — not a live simulation.
  **[recommended: yes]**
* **D-phys-3**: the bar is judged over the **whole rotation** (worst phase), not just the
  picture on screen. **[recommended: yes]**
* **D-phys-4**: the bar itself: R-a (no <1px anywhere) + R-b (rest phase: no <13px) + R-c (over
  the rotation, <13px only near edge-on) + R-d (no bead within 6px of a foreign spine).
  **[recommended: yes]**
* **D-phys-5 (K2, the edge-on pass)**: accept + a cue (dim/shrink the branch when
  `|cos a| < 0.15`), or the **hard** half-column fix, or accept silently?
  **[recommended: (a) cue for v1; (b) later if the transient still bothers you]**
* **D-phys-6**: how far may the solver move a branch off its even-fan angle? ±10° / ±20° / free.
  **[recommended: ±20° — the fan still reads even, the mirror pair gets room]**
* **D-phys-7**: may a *bead* slide radially (the spine takes `--d1`), or only whole branches?
  **[recommended: whole branches in v1]**

**B — the cone**

* **D-slant-1**: cap the magnification so a near branch is exactly the main line's size
  (13.00px), the far half keeps shrinking. **[recommended: yes — it is the literal ask]**
* **D-slant-2**: which tilt — (i) the roll (`rotateZ`, the cone's axis leans up-right, the trunk
  stays horizontal), (ii) bias the fan's rest direction (the aligned branch rests up-and-near,
  but no longer "straight up"), or (iii) both? **[recommended: (i)]**
* **D-slant-3**: if (i), how far? ψ = −8° / −12° / −15° — note the fit cost (the branches
  shorten again). **[recommended: pick by look, start at −10°]**
* **D-slant-4**: does the lean also apply to the flat / reduced-motion projection?
  **[recommended: yes, it is static]**
* **D-slant-5**: with the lean, the axis row and `SCENE_AXIS_FRAC` must be re-centred
  (the content's top grows by `cell·|sin ψ|` per step). **[recommended: recompute, then
  re-measure]**

## 15. Round 6 — the focus carousel: snap one branch to the top (proposed 2026-10-04)

**The ask (user, verbatim):** *"保持刚才说的右上开口的圆锥面，转动时分支行为可以优化。不再
固定分支角度间距，滚轮转动时，自动吸附对应的分支到圆锥面正上方的位置，此时不管其余有多少分支，
全部都位于圆锥面的下半部分。每一次滚轮滚动，都会让上方的分支回到下方并让一个新的分支转动到上方
并吸附。（整体还是保持圆锥面转动行为）这种情况下，下方的分支占用的空间会很小，所以把主干位置
向下放一些，为上方的分支腾出更多的显示空间"*

As I read it, four things:

1. the cone keeps the up-right opening of §14.4/B (the roll),
2. the fan is **no longer spread over the full circle**: the focused branch sits **on top** and
   the others are in the **lower half**,
3. a wheel gesture **snaps** a *new* branch to the top (the previous one goes down) — one new
   branch per gesture — while the scene still *rotates*,
4. because the lower branches then need little room, the **axis row moves down** so the focused
   branch gets more space.

This section is the research and the plan. Decisions are in §15.8.

---

### 15.1 What the interaction does today (measured)

**The wheel is a scroll proxy, and the phase is that scroll.**

```rust
fn pan_and_turn(el, delta) {          // the wheel and the drag both come here
    let before = el.scroll_left(); el.set_scroll_left(before + delta);
    add_turn((before + delta) - el.scroll_left());   // v0.5.69: the track's refusal becomes turn
    mark_scroll(el);                  // --rw-scroll = scrollLeft + turn
}
--phase = (--rw-scroll − --rw0) · --dpp      --dpp = 240 / panelWidth
```

Measured on the live `rewind` scene (CDP, a synthetic 100px wheel notch):

| before | after |
| --- | --- |
| `scrollLeft 0`, `scrollWidth 1200`, `clientWidth 1200` | `scrollLeft 0` (**pinned**), `--rw-scroll 100` |
| `--dpp 0.2` | `--phase calc((100 − 0) * 0.2)` |

* **The track fits its panel exactly** (1200 = 1200), so `scrollLeft` never moves: every wheel
  pixel becomes `turn`. This is the v0.5.69 case, and it means the *only* thing the wheel does
  on this fixture is turn the cone.
* **1 wheel px = 0.2°**, so a 100px notch = **20°**. Changing which branch is on top costs
  `120°` ⇒ **~600px of wheel ≈ 6 notches** (5–15 depending on the device's notch size).
* A **layout point** (`relayout_scene`: entering the view, picking a round, a resize) resets
  `turn = 0` and `--rw0 = scrollLeft`, so "at rest the aligned branch is at the front"
  (D-orb-8). `align` = the topmost trunk-parented branch of the **selected** (or current)
  round's fin — so *clicking a bead already re-aligns the fan* (L7).
* The drag pans the same way; a **horizontal** wheel delta is left to the browser (D9); the
  flat / `prefers-reduced-motion` path pins `--phase: 0`.

**The fan today** is the even full circle of §13.10 (per-fan ranks, `360/k`, a half-step nudge
when `k % 4 == 0`, a nested fan centred on 180°). **The fit today** is symmetric:
`axis = 0.5·h`, `r = min(axis, h − axis) − 16 − 13`, `q = r / longest` ⇒ on this panel
(`h = 418px`) `axis = 209`, `r = 180`, `q = 25.714` ⇒ a 7-round branch spans 180px up *and*
180px down.

---

### 15.2 The geometry of the new model

**The law (relative to the focus).** `align` already makes the angles rank-relative, so the law
becomes: the focused branch (rank 0) at `θ = 0` (the top); the other `k−1` spread over the
lower arc, inset by `δ` from the horizontal so that **no branch ever sits on ±90°** (edge-on):

```
root fan:   θ(0) = 0                                        (the focused branch, on top)
            θ(j) = 90 + δ + (j−1) · (180 − 2δ)/(k−2)        for k ≥ 3, j = 1..k−1
            θ(1) = 180                                      for k = 2
nested fan: θ(j) = 180 + (j − (k−1)/2) · (180 − 2δ)/max(k−1, 1)   (centred opposite the
                                                                  parent's ray; k = 1 → 180)
```

The nested form is the same arc *centred* rather than hung from the top, because a nested fan
has no "top" child — the parent's own ray is the one direction D-cone-7 forbids.

At `δ = 30°` a fan of 3 gives `0° / 120° / 240°` — **exactly today's angles** — so for the live
fixture nothing about the rest layout changes; the change is the *behaviour*, the axis and the
scale.

**The detents.** A branch is on top when `θ_j + phase = 0` ⇒ `phase = −θ_j`. So the detent set is
the `k` branch angles, walked in the fan's **rank order** (the server's `fin` order, stable
across reloads) and **wrapping** — the gesture therefore never dead-ends, which is what
D-cone-8/v0.5.69 was really about ("an unbounded gesture must produce an unbounded, wrapping
angle"). The cone does keep rotating — between detents it is a real turn, at a detent it is at
rest.

**The win, quantified — and the three things it does not win.** Modelled on the live fixture at
all three detents with `δ = 30`, `f = 0.72` (`q 38.9px`, the unfocused scale `s 0.65`):

| detent (focused) | the angles | edge-on | closest pair **excluding** the known K1 pair |
| --- | --- | --- | --- |
| fin0 (rank 0) | `0 / 120 / 180 / 240` | none | **4.3px** `fin2 r22` × `fin3 r23` |
| fin2 (rank 1) | `0 / 60 / 120 / 240` | none | **6.7px** `fin0 r14` × `fin3 r23` |
| fin3 (rank 2) | `0 / 120 / 240 / 300` | none | **6.7px** `fin0 r14` × `fin2 r22` |

* **Won:** no detent has an edge-on branch, so the §14.1 **K2** class (a branch's beads on the
  trunk row, 1.1-3.6px from a trunk bead at the edge-on phases) is gone at rest. The promotion
  itself still crosses the edge-on zone once per snap (17.2° of the 120° turn ⇒ ≈ 45ms of a
  300ms animation) — a transient, not a rest state.
* **Not won, (a):** the **K1** exact coincidence — modelled at all three detents, the nested
  `fin1 r21` × `trunk r31` is still **0.0px**. The law cannot move a lone nested child off 180°
  (that is its parent's ray, D-cone-7).
* **Not won, (b):** the **K3 mirror pair comes back by construction.** "The others in the lower
  half" *symmetric about straight-down* makes the two lower branches mirror images — and two
  branches whose beads share a column (here fin2 and fin3 both start at column 13) then land on
  the same row: measured today at 6.1px, modelled at **4.3px** with the unfocused scale (the
  scale shrinks the very `cos` that separates them). So the lower half is *not* collision-free:
  it needs either an asymmetric bias in the arc, a **column-aware** angle (a branch's angle
  depends on which columns it owns — which is legitimate, the law is already per-fan), or the
  relaxation.
* **Not won, (c):** the two lower branches are also 6.7px from a *nested* bead when the aligned
  branch is not rank 0.

**Therefore the carousel and §14.3's relaxation are one change, not two.** The carousel supplies
the *targets* (the `k` detents); the relaxation resolves what is left at each of them, so it runs
**once per layout per detent** (`k` solves instead of one, still a few ms in wasm) and its phase
set Φ is simply the detent set. That also *simplifies* §14.3: no 72-phase sweep is needed, the
objective is judged at the `k` rest states — and **the relaxation should land first**, because it
is what fixes the defect the user actually reported.

**What it does not fix.** The **K1 exact coincidence** is about the *nested* case
(`po = out`, a child of its parent's first bead) and is independent of this law: the lone nested
child at 180° would still land on the trunk row. That one still needs §14's relaxation (or an
inset that also excludes 180, which would put the child on its parent's ray — forbidden by
D-cone-7).

**A hard fact: "all the others below" is only possible for `k ≤ 3`.** Write the fan's branches
as points on the circle. After promoting *any* rank to the top, every other branch must land in
`(90°, 270°)`; that means every forward gap between consecutive branches must exceed 90°, and
the gaps sum to 360° ⇒ at most three branches. For `k ≥ 4` **some branch must stay above the
trunk** at some detents, whatever the spacing. (Every live session has `k ≤ 3` — `rewind` is
3 + one nested child, `Time inject` 1, everything else 0 — so this is a future concern, but it
must be decided, D-snap-6.)

**The top branch is exactly the main line's size.** At a detent the focused branch has
`a = 0° ⇒ z = 0 ⇒ k = 1` — measured today: the aligned branch's dots are exactly 13.00px. So
the "keep the focused branch the main line's size" ask of §14.4/B1 is satisfied *by
construction* at the top, and the cap only has to tame the branches that are *passing*.

---

### 15.3 Space: the axis moves down, the unfocused branches shrink

**The asymmetric fit.** Instead of "the smaller half fits the whole circle", two rooms:

```
focused  (up):    q · longest                 ≤ f·h − 16 − 13
unfocused (down): q · longest · s · max|cos θ| ≤ (1−f)·h − 16 − 13
```

with `f = SCENE_AXIS_FRAC` (0.50 today) and `s` the unfocused branches' scale. For the live
panel (`h = 418`, `longest = 7`):

| f | q (focused length) | vs today | s (live: `max|cos θ| = 0.5`) | bead spacing `s·q` |
| --- | --- | --- | --- | --- |
| 0.50 (today) | 25.7px | — | 1.00 | 25.7px |
| **0.68** | **36.5px (+42%)** | | 0.82 | 29.9px |
| **0.72** | **38.9px (+51%)** | | 0.65 | 25.3px |
| 0.80 | 43.6px (+70%) | | 0.36 | 15.7px — tight |

(`q = (f·h − 29)/longest`, `s = ((1−f)·h − 29) / (q·longest·max|cos θ|)`.)

So `f ≈ 0.68–0.72` is the sweet spot: the focused branch grows 40–50%, and the unfocused ones
are drawn at 0.65–0.82 with their **dots counter-scaled back to 13px** (the same trick the flat
projection already uses, so they stay readable and clickable, and **consecutive** dots on one
branch stay ~25–30px apart). At `f = 0.80` the compression starts to squeeze the beads too close.
Note that this budget only covers *within* a branch: the *between*-branch collisions of §15.2
are a separate constraint, and the scale makes them slightly worse, not better. "下方分支占用空间小" then holds *by construction*, and the scale also
**doubles as the focus cue** together with the existing depth dim (D-cone-10).

**The roll composes.** With the §14.4/B roll the focused branch points at the *cone's* top
(screen-tilted by ψ) and still sits at `z = 0`, so it is still exactly 13.00px; the roll does
not change any of the numbers above except that the vertical extent uses `cos ψ`.

---

### 15.4 The snap mechanics

**The target rule (keeps v0.5.69's "a gesture is never a no-op").** On settle, the phase goes
to the **next detent strictly in the direction of travel** (positive wheel ⇒ the smallest
detent ahead; negative ⇒ the largest behind). A small notch therefore always advances **at least
one** branch — "a new branch turns to the top" — and a long drag advances to the next detent
ahead of wherever it ended, so nothing is ever swallowed or double-counted.

**The animation.** A short `requestAnimationFrame` loop that writes `turn` and calls
`mark_scroll` — exactly the existing single-value pattern (`--rw-scroll`), no Leptos
reactivity, no `scroll-snap` (the proxy owns the offset, and native snap only understands real
scroll positions), no animated custom property. ~300ms, eased; **skipped entirely under
`prefers-reduced-motion`** (the flat path already pins `--phase`, here it just jumps).

**Settle detection.** A debounce after the last wheel/drag event (~120ms) — the drag's
`pointerup` settles immediately.

**What drives the focus (the one interaction question).** Today the wheel's vertical delta pans
the line and its leftover turns the fan. Three ways to attach the snap:

* **(a) focus-only wheel** — the vertical delta advances the detents (one notch ⇒ the next
  branch), the pan stays on the drag, the horizontal delta and the scrollbar. Clean and literal
  ("每一次滚轮滚动…"), and on the live fixture **nothing is lost**: the track fits, so the wheel
  cannot pan there anyway. On a wide session the wheel would stop panning the timeline.
* **(b) pan + snap** — the wheel keeps panning (D7/D9 unchanged) and the settle snaps forward.
  A notch then keeps its 20° and the *settle* does the branch change; the timeline pan survives
  on every session.
* **(c) both** — a plain vertical wheel focuses, shift+wheel pans. (More state, less obvious.)

I recommend **(a)**, with the pan kept on the drag/horizontal delta/scrollbar (it is the literal
ask, it is already what the fixture does, and it removes the "pan and turn at once" confusion).
**(b)** is the fallback if the wheel's panning is wanted on long sessions.

**The reset.** A layout point (enter the view, pick a round, resize) settles on the detent of
the **aligned** branch (the selected round's top ancestor, else the current round, else rank 0)
— D-orb-7/8 unchanged: entering the view shows a clean, snapped scene.

**Selection.** The focus can be a *view* state (the fan snaps, the panel's selected round does
not change) or the *selection* can follow the top branch (the panel then walks the branches with
the wheel). D-snap-7.

---

### 15.5 What it supersedes, what it keeps, what it costs

**Supersedes (needs the user's word):** D-fan-1 (the full-circle `360/k` step → the top + lower
arc), D-fan-3's half-step nudge (no detent is edge-on any more — kept only for the `k ≥ 4`
fallback), D-fan-4 (a nested fan is the same half-arc, centred opposite its parent's ray),
D-fan-5 (240°/panel width → one branch per gesture), D-orb-8 (continuous rest → detent rest),
and the *spirit* of D-cone-8's full circle (bounded again — but the **walk wraps**, so the
v0.5.69 lesson holds: no gesture is ever a no-op, nothing parks forever).

**Keeps:** the straight ray from the parent's bead (D-cone-6), the dim (D-cone-10), the
asymmetric fit law (D-cone-9, generalised), and all of §14's constraints C1–C5. The §14
relaxation is still needed for K1 (the nested exact coincidence), and the size cap of §14.4/B1
still matters for the *travelling* branches.

**Costs / risks:** for `k ≥ 4` "all others below" is impossible (D-snap-6); the unfocused
branches are smaller (mitigated by the counter-scaled dots); the fan is no longer a rigid
rotation of one fixed shape (the angles are re-derived per focus — which is how `align` already
works); the animation writes `--rw-scroll`, so the snap must update `turn` too or a later
`mark_scroll` would jump; a very fast wheel could produce a queue of snaps (the animation must
be interruptible and the pending target recomputed).

---

### 15.6 Verification plan

| id | check |
| --- | --- |
| **S1** | at every detent (all `k` of them, driven by the wheel), each branch's angle is `θ_j + phase` with the focused one at `0 ± 0.5°` |
| **S2** | at every detent, **no** branch is within 8.6° of ±90° (edge-on) — the K2 class is gone at rest |
| **S3** | at every detent, no two beads are < 13px apart — this **cannot** pass on the carousel law alone: today 2-6 pairs, modelled at the detents 4.3-6.7px between the two lower branches plus the 0.0px K1 pair. It is the *relaxation's* check (§14.3 run per detent), so S3 is the gate for shipping the two together |
| **S4** | one small wheel notch (≤ 40px) always changes the top branch (v0.5.69's no-op rule) and the walk wraps `k → 0` |
| **S5** | the focused branch's dot measures 13.00px and its length grows by the fit's ratio (≥ +20% at `f = 0.72`); the unfocused dots also measure 13.00px (counter-scaled) |
| **S6** | the axis row is at `f·h` (`|Δ| < 0.5px`) and the trunk's beads stay exactly on it |
| **S7** | with `prefers-reduced-motion`, the phase jumps to the detent with no animation, and the flat projection is static |
| **S8** | a straight-ray check at every detent: each spine's endpoints coincide with its first/last bead (the scale must not bend anything) |

The existing suite (48 + 68 + 8 + 140) runs unchanged; `L7` (click-to-align) must now land on a
detent, which is exactly what it asserts.

---

### 15.7 Step plan (once the decisions land)

0. **O-snap-0** — §14.3's relaxation, run **per detent** (it is what clears K1 and the lower
   branches' mirror pair), on top of today's even fan.
1. **O-snap-1** — the fan law becomes the focus carousel law (`fan_root_theta` /
   `fan_nested_theta` rewritten; the assertions in `fan_angles_tests` updated: the focused one
   at 0, the rest inside `(90+δ, 270−δ)`, `k ≤ 3` provably all-below).
2. **O-snap-2** — the detent model: the focus rank, `phase = −θ_focus`, the walk order and the
   wrap; a `relayout_scene` lands on the aligned branch's detent.
3. **O-snap-3** — the gesture: the settle rule (the next detent in the direction of travel), the
   rAF animation writing `turn` + `mark_scroll`, the debounce, the reduced-motion jump, and the
   wheel's new role (D-snap-2).
4. **O-snap-4** — the asymmetric fit (`SCENE_AXIS_FRAC`, the two rooms) and the unfocused scale
   `s` (the container `scale` + the beads' counter-scale, reusing the flat projection's
   mechanism), then re-measure and pin the numbers.
5. **O-snap-5** — the roll/tilt of §14.4/B on top (if approved), the flat law and the axis
   re-centre.
6. **O-snap-6** — the probes S1–S8 (new probe + `orbit_probe.py` deltas), the design doc's
   §3b.6/§6, the mirror's canaries, both repos, a version bump.

Everything stays **client + stylesheet** — the server projection does not change.

---

### 15.8 Open questions for the user

* **D-snap-1**: accept the law — focused at the top, the rest in the lower arc inset by `δ`
  (so no detent is ever edge-on)? **[recommended: yes, δ = 30°]**
* **D-snap-1b**: the lower arc is *symmetric* today (k = 3 → 120°/240°), which is exactly what
  makes the two lower branches mirror each other and collide (4.3px modelled). Break the
  symmetry — (i) a fixed bias (the lower branches at 90+δ and 270−δ−b, b ≈ 15°, i.e.
  135°/225°), (ii) a **column-aware** angle (the arc's spacing follows each branch's own
  column span, so branches that share columns are pushed apart), (iii) leave it to the
  relaxation? **[recommended: (iii) + (ii) as the relaxation's first move — measure both]**
* **D-snap-2**: the wheel's new role — (a) focus the branches (the pan stays on the drag /
  horizontal delta / scrollbar), (b) keep panning + snap on settle, (c) shift+wheel pans?
  **[recommended: (a) — on the live fixture nothing is lost]**
* **D-snap-3**: the **no-op rule** — every gesture advances at least one branch, and a long
  gesture lands on the next detent *ahead* (not "the nearest"). **[recommended: yes]**
* **D-snap-4**: the animation — ~300ms eased, a rAF writing `--rw-scroll`; instant under
  `prefers-reduced-motion`. **[recommended: yes]**
* **D-snap-5**: the axis `f` and the unfocused scale `s` — `f = 0.68 / 0.72 / 0.80`, with `s`
  derived from the fit (the focused branch grows 23% / 51% / 74%). **[recommended: f = 0.72,
  and pick by look]**
* **D-snap-6**: `k ≥ 4` (where "all others below" is geometrically impossible): (i) allow the
  minimum number of branches above the trunk, spread as low as possible; (ii) fall back to the
  even full circle for `k ≥ 4`; (iii) keep the current `k % 4` nudge. **[recommended: (i),
  and document it]**
* **D-snap-7**: does the wheel change the **selection** (the detail panel walks the branches) or
  only the **view** (the panel keeps the selected round)? **[recommended: view only — the panel
  must not churn while browsing; a click still selects]**
* **D-snap-8**: the nested fan keeps the same law centred on its parent's opposite ray (a lone
  child at 180° as today, and §14's relaxation fixes its exact coincidence with the trunk row).
  **[recommended: yes]**
* **D-snap-9**: keep the §14 roll (the up-right opening) on top of this, and does its ψ apply to
  the *detent* orientation too (the focused branch then points at the cone's top, which is
  screen-tilted by ψ — not straight up)? **[recommended: yes to both, ψ = −10°]**
* **D-snap-10**: the supersession list of §15.5 (D-fan-1/3/4/5, D-orb-8, and D-cone-8's full
  circle bounded again while the walk keeps wrapping). **[recommended: accept]**

---

## §15.9 As built (v0.5.73, 2026-10-05)

Shipped as the default, exactly as §15.8 recommends, with the three
corrections below — all three are **measurements**, taken on the live fixture
(`rewind`, 34 cols, 418px scene, 3 root branches, a nested child on the
parent's first bead), not preferences.

**The constants that landed**

| what | value | measured effect |
| --- | --- | --- |
| `SCENE_AXIS_FRAC` | **0.68** (not 0.72) | `q 25.7 → 36.5px` (**+42%** on the focused branch), `--kof 0.82` |
| `FAN_ARC_INSET` (δ) | 30° | no unfocused branch is edge-on; the live detents are `120°/0°/−120°` |
| the unfocused slope | `--ql = --q · --kof`, `--kof = 0.82` | the lower arc fits; every dot still 13px (the focused one exactly 13.00) |
| `NESTED_SHIFT` | **2 steps** | K1 across all detents: `0.0px → 44/29/29px` |
| `CONE_ROLL_DEG` | **0** (wired, inert) | see below |

**Correction 1 — the axis is 0.68, not 0.72.** §15.3 picked `f = 0.72` on the
scale numbers alone. Live, 0.72 makes `--kof` collapse to 0.65, and then the
**first bead of an end-of-arc branch sits 12.7px from the trunk's own bead in
the same column** — a hair inside one dot (measured 7.7/9.2px once the
perspective and the roll were applied). At 0.68 the same distance is 15px
(clear) for 8px less reach. Both are modelled two ways (the analytic model and
the live DOM) and agree.

**Correction 2 — a nested fan starts **two** steps out, and this is the whole
of K1's fix.** §14.1's K1 was diagnosed as a solver problem; it is not. K1 is
exact and **phase-independent**: with `po == out` both `(po − out)·q` and
`−(po − out)·q·sin a` vanish, so the child's bead is on the trunk row at every
phase and no relaxation of *angles* or *radii* can change that (only moving
the child itself). `out = po + 2` cannot cancel for any phase. Two, not one:
at one step the bead is 8.7px from a sibling branch's bead at the detent where
that sibling is focused; at two it is ≥ 20px from everything at all three
detents. The branch's reach (`data-n`, `--n`) grows with it, so the fit and
the spine stay correct — verified: the longest branch's beads are 0.1px off
its spine and its last bead is at 99.7% of it.

**Correction 3 — the up-right tilt must not be a `rotateZ`.** D-snap-9's roll
is the wrong mechanism and was shipped **off** (`CONE_ROLL_DEG = 0`, the CSS
wired and inert). `rotateZ(ψ)` on a branch's plane rolls that branch's **own
column axis** as well: a 7-column branch's bead line then spans
`±7·cell·sin ψ` = 41px at ψ = 10°, which tilted the bead line and lifted its
far beads **above the trunk row** — the probe caught unfocused beads on the
wrong side of the axis at three of the four steps. What the user asked for
("the cone's opening leans up-right") is a **shear of the radial direction
only**: each step leans, the columns stay horizontal. That is §14.6's D-slant
question (O-slant-1..3), and it stays open.

**Verified (all green, live, 2026-10-05 00:42)**

* `/tmp/carousel_probe.py 8480 rewind` — **0 failed checks** over S1–S6: at
  every detent exactly one branch is focused, the detent phase is exactly 0,
  **every unfocused root branch's beads are below the axis**, no branch pair
  coincides, the focused branch's dots are exactly 13.00px (the rest
  11.7…13.2px, the perspective's band), the axis is `0.68·h`, `q = 36.5px`,
  `--kof = 0.82`, and 3 wheel notches walk `fin0 → fin2 → fin3 → fin0` —
  every notch a *new* branch up, the walk wrapping.
* `/tmp/spine_probe.py 8480 rewind` — the spine still ends on its last bead
  (a 7-round branch: beads 0.1px off the line, the last at 99.7%).
* `cargo check --target wasm32-unknown-unknown` clean; `trunk build`
  succeeds; `cargo test -p rushi-web` unchanged; the two native law tests
  (extracted, `rustc --test`) pass.

**Correction 4 — the wheel keeps a pan fallback.** D-snap-2 says the vertical
wheel is the focus control. On a fan of **0 or 1 branch** there is nothing to
bring up (a lone branch is always the focused one under this law, which
supersedes D-cone-11's "a lone branch orbits like any other"), so the wheel
falls back to the pan it had before the carousel — and it has to go through
the same **proxy** v0.5.69 needed: the scroller is `overflow-x` only, so a
vertical delta scrolls nothing by itself (measured: `260 → 260` with the event
left alone). Without this, the wheel was a **no-op on every live session but
`rewind`**, which breaks v0.5.69's rule — `flow_style_b_probe`'s F4 caught it.

**Still open (ranked)**

1. **K3, the mirror pair (8.3px measured)** — the two one-bead lower branches
   of the live fixture are mirror images on one column, and the symmetric arc
   is *why the walk is jump-free*: any asymmetric placement of a `k = 3` fan
   teleports branches at the detent (proved in the model: a 180–240° swing).
   The options, in the order §15.7 stages them: (a) **O-snap-0**, the §14.3
   relaxation run **per detent**, which may only nudge within its cap — it
   cannot fix a mirror pair *by angles* either, so it must be given a lateral
   DOF; (b) **D-snap-1b(ii)**, a column-aware **x-stagger** of a half cell for
   branches sharing a column (a static assignment keeps it jump-free; the cost
   is that the focused branch can be 17px off its own columns); (c) accept it
   (8.3px of overlap between two 13px dots at the *bottom* of the cone, the
   dimmest, smallest, most "background" pair in the scene).
2. **The up-right tilt** (Correction 3) — the shear form, D-slant.
3. `k ≥ 4` is unbuilt and unmeasurable here: **no session on this host has
   more than 3 root branches** (`flow_check.py`: every other session has 0 or
   1). D-snap-6's "allow the minimum above the trunk" is written but never
   exercised — the first real `k ≥ 4` session must be looked at before the
   rule is trusted.
4. The **hardened** relaxed-state question of §14.5 R8: with the roll off,
   nothing in the current build depends on `--roll` being non-zero.

---

## §16 Round 5 — the three defects the user found in v0.5.73 (2026-10-05)

Reported after looking at the shipped carousel, verbatim:

1. **"转动吸附到上方后，分支会闪一下然后渲染到动画停止点附近的位置"** — after a
   branch snaps to the top it *flashes*, then renders *near* where the flight
   stopped (i.e. not exactly where the flight ended).
2. **"abandon 分支的数字不要用横线划掉，会导致用户看不清楚数字"** — the round
   number of an abandoned branch must not be struck through; the line makes the
   number unreadable.
3. **"节点 14 到节点 21 的处理方式还是不好，14 吸附在主干上方时，21 节点被渲染
   到了主干下方。二级子分支的圆锥面角度可以小一点，至少被聚焦的分支无论有多少
   节点和子分支都应被渲染在主干上方"** — the 14 → 21 case is still wrong: when
   round 14 is snapped above the trunk, round 21 (its child) renders *below*
   the trunk. The second-level cone angle should be **smaller**, and at the very
   least **a focused branch — however many nodes and sub-branches it has — must
   render above the trunk**.

All three are diagnosed below with live measurements; §16.4 is the order and
the verification, §16.5 the decisions that are the user's.

### 16.1 F1 — the snap's *scale* pop (the "flash")

**Measured, not guessed.** A rAF sampler (`/tmp/snap_flash_probe.py`, 150 frames
at ~16 ms over one wheel notch on the live `rewind` session) records, per frame,
the phase, every root branch's `--th` and the screen `y` of its topmost bead.
The flight itself is clean — the frames around the commit read:

```
t=312ms  phase=119.6   fin0*: th   0.0  top 587   fin2: th-120.0  top 541
t=323ms  phase=119.6   fin0*: th   0.0  top 587   fin2: th-120.0  top 541
t=331ms  phase=  0.0   fin0 : th 120.0  top 584   fin2*: th   0.0  top 534
```

The *angles* are continuous across the commit (fin0 `0 + 119.6 → 120 + 0`,
fin2 `−120 + 119.6 → 0 + 0`: **≤ 0.4°**), which is the property §15.4 was built
for. What is **not** continuous is the **radius**: `--ql` is keyed to the
*focus* — `--ql: var(--q)` on a `data-focus=1` branch and `--ql: calc(var(--q) *
var(--kof))` on `.rw-branch.unfocused` — so the branch that has just *arrived*
jumps from `q·kof` to `q` in the single frame the class flips. On the live
fixture: fin2's out=1 bead moves `29.9px → 36.5px` from the axis (**+6.6px**, the
measured 541 → 534) and the departing fin0's beads shrink by the same 22%. That
is the flash — and it is exactly why the branch lands *near* the flight's end
point instead of *on* it: the flight moves the angle; the class flip then moves
the radius.

**Fix F1 — the slope becomes a function of the angle, never of the focus.**
`--abs` already holds every branch's *absolute* angle (root or nested, phase
included), so:

```css
--ql: calc(var(--q) * (var(--kof) + (1 - var(--kof)) * max(0, cos(var(--abs) * 1deg))));
```

* on top (`abs = 0`) → `cos = 1` → `--ql = --q` — **identical to today at rest**;
* below the horizontal (`|abs| ≥ 90°`) → `max(0, …) = 0` → `--ql = q·kof` —
  identical to today for every unfocused branch (the live lower arc is ±120°);
* in between, and during a flight, it is **continuous**, so the arriving branch
  grows *smoothly* as it rises and there is no frame where its radius jumps.
  A branch below the axis is unaffected (`cos < 0` there), so the down-fit that
  `--kof` encodes still holds; a branch *above* is bounded by the full `q`,
  which is what the up-room was sized for.

The `.rw-branch.unfocused { --ql: … }` rule and the class go (the render keeps
`data-focus`, which the probes read). The mirror's selector note
(`install/TOUCHPOINTS.md`, v0.5.73's paragraph) has to be updated with it.

*Invariant to add:* the slope is a continuous function of the branch's own
angle; nothing about a branch's *size* may change discretely with the focus.

### 16.2 F2 — the abandoned round's number keeps its line

`web-leptos/style.css:1843`:

```css
.rw3-node.abandoned .rw3-round { opacity: .6; text-decoration: line-through; }
```

`.rw3-round` *is* the number in the flow scene's bead, so the strike hits exactly
what the user cannot afford to lose. **Fix:** drop the `text-decoration`; keep
the muted colour and the `.6` opacity as the "abandoned" cue (and the dot keeps
its own `.45` grey, which is the primary cue today).

Related, and deliberately *not* changed without a word from the user:

* the **list** style strikes the *summary*, not the number —
  `.rw-node.abandoned .rw-sum { text-decoration: line-through }` (1718) — and
  dims the whole head to `.55`. The number there is legible.
* the **transcript**'s `.ev-retract` (711) strikes a retracted *event card*'s
  text. Different surface, different meaning.

### 16.3 F3 — a focused branch's sub-branches must stay above the trunk

**Measured today.** The carousel probe already reports it as data: at *every*
detent `below_nested: [1]` — the nested fan's beads are **always below the axis**
— and with fin0 (round 14) focused the 21 bead sits **53px below** the trunk row
while its parent's beads run up to 255px above it. Mechanically: the container
is `po = 1` step up the parent's ray, and the child's first bead is 3 steps out
along **180° relative to the parent** ⇒ `y = −po·q − out·q·cos 180 = −36.5 +
89.8 = +53px` below the trunk.

**Root cause** is a v0.5.72 decision, **D-fan-4**: "a nested fan is centred on
**180°** — opposite its parent's own ray, never along it". That was chosen to
keep a child off its parent's beads (K1/K2 neighbours), but with the carousel a
focused parent points **up**, so "opposite the parent" points **down, through
the trunk**.

**Fix F3 — the nested fan opens along the parent, not against it (supersedes
D-fan-4's centre).** `fan_nested_theta` centres the fan on the parent's own
direction (`0°`, not `180°`) with a **small** spread, `±NESTED_HALF` and a small
bias so that no child is exactly collinear with its parent (the spine would
overlap) and none is edge-on:

* `NESTED_HALF ≈ 40°`, so **|θn| < 90°** — and *that* is the structural
  guarantee the user asked for: with the parent at `abs = 0` (focused), every
  descendant's `cos > 0`, so **every step of the subtree is upward**, at any
  depth and for any number of children. No per-fixture arithmetic, no cap.
* Live numbers for round 21 with a 20° bias: its bead becomes
  `−36.5 − 3·36.1·cos 20° ≈ **138px above** the trunk` (from 53px below), and it
  stays clear of its parent's own beads in the same column (the parent's round
  15 sits at −73px, the child at −138px: **65px** apart).
* A lone child is the interesting degenerate case: at exactly `0°` its spine
  lies *on* its parent's for the common length. The bias fixes that; the
  decision in §16.5 is which bias.

**Two consequences F3 drags along (both must land with it):**

1. **The fit must measure the *chain* reach, not the local one.** The layout's
   `longest` is the maximum `data-n` over `.rw-branch`, and `data-n` is a
   branch's **own** reach (`n + shift`). A nested branch's distance from the
   trunk is its **ancestors' `po` plus its own reach** — while a nested child
   pointed *down* that error was hidden by the generous lower room, but a
   subtree that goes *up* must fit the upper room. Fix: write `data-reach`
   (absolute steps from the trunk, accumulated through the chain) in the render
   and use it for `up_room / longest`. On the live fixture the deepest reach is
   `1 + 3 = 4` steps against the longest root branch's 7, so **no number moves
   today** — it is correctness for the general case.
2. **The focused subtree now rides the full slope**, because F1's angle-based
   rule gives `cos ≈ 1` at `abs ≈ 0`. That is the wanted behaviour (the subtree
   keeps its 2-step clearance from its parent instead of shrinking), and it is
   also why consequence 1 is not optional.
3. Copy to update: design doc §3b.6 (D-fan-4's nesting entry + the carousel
   entry), the mirror README, `install/TOUCHPOINTS.md`.

### 16.4 Order, verification, effort, rollback

Land them as one change, in this order (each is independently verifiable):

1. **F2** (one CSS line) — no geometry involved.
2. **F1** (CSS + the class removal) — verify with the frame sampler: per frame,
   for every branch, the bead radius from the axis must change by no more than a
   smooth bound (and by **0** across the commit frame), while the flight still
   ends exactly on the detent. `carousel_probe.py`'s S1–S6 must stay green
   (at rest the formula is identical) and `flow_style_b_probe.py` 68/0 must hold.
3. **F3** (Rust law + `data-reach` + the fit) — new probe checks, all on the
   live session, at **every** detent:
   * the focused branch's **whole subtree** (its own beads *and* every nested
     descendant's) is **above** the axis;
   * no bead of the subtree is within 13px of a trunk bead, or of another bead
     of the same subtree;
   * the nested bead's clearance from its parent's beads in its own columns
     (the K1/K2 family) is ≥ 13px;
   * the existing S1–S6 (one focus, phase 0, unfocused roots below, dots 13px,
     the walk wrapping) stay green.

Then: `cargo test -p rushi-web` (71), the wasm check + `trunk build`,
`flow_check.py` (8/0), `rewind_probe.py` (140), rebuild `dist/`, mirror sync
(canaries), README/TOUCHPOINTS and a commit. **No tags** — the user removed
them (2026-10-05): the rollback points are the **commits**,
`pre-carousel` = `0d4752d` (§15's last docs commit) and `carousel` = `0525fce`
(v0.5.73), and round 5's own is `ec67918` (v0.5.74). `git checkout` one of
those hashes is the whole rollback.

Rough effort: F2 ≈ 5 min, F1 ≈ 45 min (mostly the sampler), F3 ≈ 2 h (the law,
the fit's reach, the probes, the docs).

### 16.5 The decisions that are the user's

1. **The lone nested child's bias** (§16.3): exactly `0°` (parallel to the
   parent — reads as a continuation rail, but its spine overlaps the parent's
   for the common length) or a small `±φ` with `φ ≈ 20–30°` (visibly separate,
   still well inside the ±90° guarantee). **[recommended: φ = 20–25°, chosen so
   the two spines never overlap on screen]**
2. **The nested spread** `NESTED_HALF`: 40° (recommended — the smallest that
   still reads as a cone) or narrower (25–30°) now that the user asked for
   "小一点". **[recommended: 40°, tune by look]**
3. **F2's scope**: only the bead's number (recommended, that is what was
   reported), or also the *list* style's abandoned **summary** strike
   (`.rw-node.abandoned .rw-sum`)? **[recommended: leave the summary — it is
   prose, not the number; the user's words were about numbers]**
4. **F1's visual**: with the fix, the arriving branch *grows* smoothly as it
   rises (its beads slide outward). That is the natural consequence of making
   the slope continuous; if the user would rather it arrive at its final size
   only *after* the flight, the alternative is to animate `--kof` per branch —
   more code, and it would reintroduce a (smaller) pop. **[recommended: keep the
   smooth growth]**

### 16.6 As built (2026-10-05) — v0.5.74

All three landed as one change. Each number below is measured on the live
`:8480` fixture (`rewind`), on the build the tag points at.

**F1 — the slope is now a function of the angle** (`style.css`): `--ql:
calc(--q · (--kof + (1 − --kof) · max(0, cos(--abs))))`; `.rw-branch.unfocused`
and the class are gone (`data-focus` stays, the probes and the fit read it).
The commit frame's worst radius move is **0.20px** (was **7px**: the arriving
branch's out=1 bead 541 → 534 under the old class flip) and the largest
single-frame move anywhere in the flight is 6.1px, i.e. smooth. Resting sizes
are bit-identical to v0.5.73: focused `−q` (−36.42px), lower arc `+q·kof`
(+13.36px at ±120°). Verified by `e2e/snap_probe.py`.

Two things F1 dragged in, both required for correctness:

* **A nested container's offset must ride its *parent's* slope.** The child sits
  `--po` steps along the parent's ray, so it needs the parent's `--ql` — which a
  custom property defined on the child cannot express (a rule's own definition
  wins on that element). The render therefore also writes **`--sum-par`** (the
  chain's accumulated angle up to but excluding this branch), so the stylesheet
  computes `--qlp` from `--aroot + --sum-par` — exactly the parent's angle — and
  the nested container's `top` uses `--qlp`. Measured: the child's container
  origin lands on the parent's fork bead at every detent (541 vs 540.6 at the
  focused one).
* The bead/spine rules keep reading the **local** `--ql`, so a subtree scales
  with its own angle.

**F2 — the number keeps its line**: `.rw3-node.abandoned .rw3-round { opacity:
.6 }` (the `text-decoration` is gone). The list style's abandoned *summary*
strike (`.rw-node.abandoned .rw-sum`) is deliberately untouched, per the user.
A new invariant is on the record: a round number is never struck through.

**F3 — the nested fan opens along its parent**: `fan_nested_theta` is a
one-sided fan, `NESTED_BIAS_DEG` = **15°** (the *user's* 22° did not survive
measurement — see the table in the source: at the mirrored detent a bias that
pushes the child toward ±90° makes its plane nearly edge-on and its beads
compress into the parent's own band; the measured safe window is 12–18°, and 15
is its centre), `NESTED_HALF_DEG` = 25° (the user's number) and
`NESTED_CHAIN_MAX_DEG` = 70° — a chain whose accumulated angle would overshoot
scales its fan down (`fan_nested_theta_in`), so **|θ| < 90° at every depth** and
the "focused subtree climbs" guarantee holds for any nesting. The fit now reads
**`data-reach`** (the chain-absolute reach the render writes) instead of
`data-n`, so a climbing subtree is paid for by the upper room.

Measured on the fixture: with fin 0 (round 14) focused, **round 21 renders
134px above the trunk** (was 53px below it) at 17.3–39.4px from its parent's
beads across all four detents, and the focused branch's whole subtree is above
the axis row at every detent. `e2e/spine_probe.py`: the nested spine's beads are
0.8px off its line, ending at 99.5% (root branches 0.0–0.3px / 99.7%).

**Probe corrections found while verifying (all pre-existing):**

1. `carousel_probe.py` compared viewport bead rects against the **scene-local**
   `--axis` px value, so every "below the axis" assertion was vacuously true;
   the sampler now computes the axis in the same coordinates
   (`scRect.y + --axis`), and S1/F3 are real checks.
2. The same probe's `below` set did not exclude the **focused** branch's beads
   (they are *supposed* to be above), and its old `b.fin !== 'trunk'` filter
   never matched — the focused set is now filtered by the container's focus.
3. `spine_probe.py` projected beads onto an arbitrary bbox diagonal — the *wrong*
   one for an up-right bar (154px of phantom error). It now scores both
   diagonals and takes the fit, and scopes beads to `:scope > .rw3-node`.
4. `flow_style_b_probe.py`'s H12 asserted D-fan-4 (a child at 90–270°) but the
   fixture it runs on has no nested fan, so it passed vacuously; it now asserts
   the F3 law (0 < |θ| < 90) and says where the real proof lives.
5. `snap_probe.py` (new) is F1's check — the commit frame's radius continuity.

**Verification (all green, live, 2026-10-05, v0.5.74):** `carousel_probe 8480
rewind` **0 failed** (S1–S6 + the two new F3 checks at all four detents);
`snap_probe` **PASS** (0.20px); `spine_probe` 0.0–0.8px, 99.5–99.7%;
`flow_style_b_probe` **68/0**; `flow_check` **8/0**; `rewind_probe` **140 PASS**;
`cargo test -p rushi-web` **71 passed**; wasm check + `trunk build` clean; the
extracted native law tests **3 passed** (including the new
`a_focused_subtree_always_climbs`).

**Open, recorded:** the K3 mirror pair of the two one-bead lower branches
(**9.1px**, `fin2 r22 × fin3 r23`) is unchanged — it is the symmetric arc's own
problem (§15.9) and no round-5 change touches it; `NESTED_HALF_DEG` = 25° means
a nested fan of 3+ children would spread into the 15–40° band, where the
clearance measurement only covers the 15° edge (no live session has one);
`k ≥ 4` roots stays unexercised.
