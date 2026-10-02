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

Extra (beyond the plan text, decided during implementation):

- The dialog is **one component** shared by the tree, the panel rows and the
  card button (the plan described the card button as "the same dialog").
- The History top bar carries the **loop lamp** (§3's sketch) as the visible
  reason the tree is read-only.
- The panel's rows are the **active path** (one click per round) rather than a
  static summary only.
- `#history-view`'s tree scroller is `#hist-tree` (plan §3 naming) and is
  registered in all five capsule-scrollbar rule lists (plugin rule 4).
