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
