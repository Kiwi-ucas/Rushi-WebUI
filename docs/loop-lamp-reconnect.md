# Loop-Lamp Reconnect — Investigation & Improvement Plan

Symptom (user): after a page refresh, sessions whose loop is running do **not**
show the orange breathing lamp until you manually click one; and if you close
the webui and come back, it "forgets" which sessions were running and whether
a loop has finished.

---

## 调研现状 (what actually happens today)

### Symptom 1 — refresh: no lamp until you click

- `active_session` is **not persisted** — it starts `None` on every load
  (`model.rs:609`) and is only set by a click (`ui.rs:513`).
- `ws::connect` is called in **only two places**: `select_session`
  (`ui.rs:537`) and the auto-reconnect path (`ws.rs:729`). **Nothing opens a
  WS on app startup.**
- The running-set snapshot for the sidebar lamps is the `loops` WS frame
  (`main.rs:490-502`, sent on every WS connect from `running_sessions()`).
  The client applies it with `*looping.write() = set` (`ws.rs:543`), which
  drives `looping_sessions` → the card class `running running-bg`
  (`ui.rs:1241`) → the orange lamp.
- **So:** fresh load → `active_session = None` → no WS opened → no `loops`
  frame → `looping_sessions` stays empty → no lamps. Clicking a session opens
  the WS, the `loops` frame arrives, and *all* running lamps light. That is
  exactly the reported behavior.

### Symptom 2 — close webui, reopen: forget which ran / whether finished

- `looping_sessions` and `loop_done_unviewed` are **client-only** signals
  (`model.rs:504,508`) — wiped on reload, and not re-seeded unless a WS is
  open.
- The server's `running_sessions()` now survives a restart via a `loop.pid`
  liveness rescan (my earlier fix to `process.rs`) — so the *running* set is
  recoverable on disk.
- **Terminated-loop state is not persisted anywhere.** The server's waiter
  task (`process.rs:331-361`) publishes `LoopEvent{running:false}` over the
  broadcast channel but never writes a "last exit" marker to disk. So after
  close/reopen there is no record of *which loops finished* — the green
  "finished-but-unviewed" lamp cannot be restored, and the user cannot tell a
  loop completed while they were away.
- Even a **WS reconnect** (not a full reload) only resyncs the *running* set
  via the `loops` frame; the *finished* set is lost.

### Current state of each piece

| Piece | State |
|---|---|
| `is_running` / `running_sessions` with `loop.pid` liveness | ✅ works (earlier fix) — survives restart |
| `GET /api/sessions/{id}/loop` endpoint | ✅ works (in-memory + `loop.pid` fallback) |
| WS `loops` frame | ✅ now carries **both** `running` + `finished` (P1) |
| WS `loop_status` frames (live start/stop) | ✅ correct while observed live |
| Client `looping_sessions` / `loop_done_unviewed` | ✅ seeded by `/api/loops` poll + `loops` frame (P0/P1) |
| `GET /api/loops` (running + finished) | ✅ P0 running, P1 added `finished` + orphan sweep |
| Per-session "last loop exit" on disk (`loop.last`) | ✅ P1: written on exit + orphan sweep |
| `POST /api/sessions/{id}/loop/viewed` | ✅ P1: deletes the marker (consumes the green lamp) |
| `active_session` persistence | ❌ P2 (not done) |
| Per-session "viewed" marker on disk | ⚠️ P1 uses an in-memory `loop_viewed` set + server marker deletion; a cross-reload disk marker is P2 |

---

## 改进计划 (improvement plan)

### P0 — make running lamps survive a reload (no click needed)

1. **Server:** new `GET /api/loops` → `{ "running": [names] }`
   (calls `loops.running_sessions()`; disk-liveness, **no WS dependency**).
2. **Client:** on startup **and** on a 10s poll (reuse the existing
   `load_sessions` poll cadence in `lib.rs`), call `/api/loops` and set
   `looping_sessions`. This lights the orange lamps for *every* running
   session even when no session is active / no WS is open — independent of
   the WS lifecycle.

### P1 — remember which ran + whether finished after close/reopen

3. **Server:** persist a per-session last-exit marker. In the waiter task
   (`process.rs:331-361`, after `child.wait()`), write
   `sessions/<id>/loop.last` = `{ pid, exit, stopped, ts_iso }`.
4. **Server:** extend `GET /api/loops` →
   `{ "running": [...], "finished": [{name, exit, stopped, ts}] }`
   (read each session's `loop.last`; optionally filter to a recency window).
5. **Client:** on startup/poll, seed `loop_done_unviewed` from `finished`
   → restore the green "finished-unviewed" lamps; seed `looping_sessions`
   from `running`.
6. **Server:** also carry the `finished` set in the WS `loops` frame, so
   finished-unviewed lamps survive a **WS reconnect**, not just a full
   reload.

### P2 — polish (optional)

7. Persist `active_session` to localStorage and auto-`select_session` on
   load → restores *which session you were viewing* (the "forget which one I
   was in" half of the complaint).
8. Add a per-session `.loop.viewed-ts` marker so a session you *already*
   viewed before closing does not re-light its green lamp.

---

## 实现记录 (Implementation log)

### P0 — DONE (v0.5.55)

Implemented per plan; built + deployed to `:8480` and verified.

- **Server** `bin/rushi-web/src/main.rs`: new `async fn get_loops` →
  `Json({ "running": st.loops.running_sessions().await })`, routed at
  `GET /api/loops`. Reuses the `loop.pid`-liveness `running_sessions()`
  fix, so it is correct across a server restart (orphaned loops).
- **Client** `web-leptos/src/api.rs`: new `pub async fn load_loops() ->
  Vec<String>` (GET `/api/loops` → `running` array; `Vec::new()` on any
  error — degrades silently, never blocks the poll).
- **Client** `web-leptos/src/lib.rs`: a new `spawn_local` poll (s4) runs
  `load_loops()` on mount and every 10 s, replacing `looping_sessions`
  with the server's running set. This is **independent of the WS** — a
  fresh load (no active session, no WS) still gets the lamps.

Verified:
- `curl :8480/api/loops` → `{"running":["Webui"]}` (only the live loop
  pid; stale `loop.pid` files for dead sessions correctly excluded).
- Served wasm `rushi-web-ui-b7b8560f…` references `/api/loops`.
- Loop process `rushi run Webui` (pid 54154) survives a server restart
  (its own process group), so the lamp stays correct.

**Green-lamp semantics decided for P1:** "until the user views that
session" — i.e. restore `loop_done_unviewed` from the server's
`finished` list on reconnect, and clear a session's green lamp on
`select_session` (matches the existing `loop_done_unviewed` behaviour
at `ui.rs` `select_session`).

### P1 — DONE (v0.5.55)

Implemented per plan; built + deployed to `:8480`, E2E-verified on the
`:8482` probe (`--loop-cmd true`).

**Server** (`bin/rushi-web/src/process.rs` + `main.rs`):
- New `LoopLast { pid, exit, stopped, ts }` record, persisted at
  `sessions/<id>/loop.last`. Written by the per-loop waiter on exit
  (normal or stopped); also written by the **orphan sweep** in
  `loops_snapshot()` for any session whose `loop.pid` is dead but has no
  marker yet (a loop that died while this server was stopped, or a TUI
  loop this webui never observed).
- `LoopManager::start()` deletes a stale `loop.last` when a fresh loop
  begins (a new run supersedes the previous result).
- New `LoopManager::loops_snapshot() -> LoopsSnapshot { running,
  finished }`: `running` = `running_sessions()`; `finished` = every
  session with a `loop.last` marker that is **not** currently running,
  newest first.
- `GET /api/loops` now returns the full snapshot (was running-only in P0).
- New `POST /api/sessions/{id}/loop/viewed` (`post_loop_viewed`): deletes
  `loop.last` **and** a stale (dead-pid) `loop.pid`, so the next
  poll/reconnect does not re-light the consumed green lamp; a live
  `loop.pid` is left alone.
- The WS connect-time `loops` frame now carries **both** arrays
  (`running` + `finished`), so a WS reconnect restores the green lamps too.

**Client** (`web-leptos/src/`):
- `api.rs`: `load_loops() -> LoopsSnapshot { running, finished }`
  (finished parsed as session-name strings); new fire-and-forget
  `mark_loop_viewed(id)`.
- `model.rs`: new `loop_viewed: RwSignal<HashSet<String>>` — sessions the
  user has viewed this session; guards against the poll/frame re-seeding
  a consumed green lamp.
- `lib.rs` (s4 poll, mount + 10 s): sets `looping_sessions = running` and
  seeds `loop_done_unviewed` with `finished − running − loop_viewed`
  (insert-only; removal is owned by `loop_status` frames + `select_session`).
- `ws.rs`: the `loops` frame handler parses the new shape and seeds the
  same way; the live `loop_status` frames are unchanged.
- `ui.rs` `select_session`: records the view in `loop_viewed` and fires
  `api::mark_loop_viewed` so the server marker is deleted.

**Verified (probe `:8482`, `--loop-cmd true`, session `beta`):**
1. `POST /api/sessions/beta/start` → `{"ok":true,"pid":…}`.
2. On exit the waiter wrote `sessions/beta/loop.last`
   (`{"pid":…,"exit":0,"stopped":false,"ts":…}`).
3. `GET /api/loops` → `beta` present in `finished`.
4. `POST /api/sessions/beta/loop/viewed` → `{"ok":true}`.
5. `sessions/beta/loop.last` gone.
6. `GET /api/loops` → `beta` absent from `finished`.

**Remaining (P2, not done):** persist `active_session` (which session was
viewed) and a cross-reload "viewed" marker so a session already viewed
before closing does not re-light its green lamp on the next open.

### Decisions (resolved)

- **Green-lamp validity:** *until the user views that session* (user
  decision) — not a fixed time window. Implemented as: server keeps
  `loop.last` until `loop/viewed` deletes it; client keeps a `loop_viewed`
  set so the poll/frame does not re-seed a consumed lamp.
- **Scope:** P0 (running lamps survive reload) + P1 (finished lamps
  survive close/reopen) are done. P2 (persist `active_session` + a
  cross-reload viewed marker) remains optional.

---

## v0.5.58 — the liveness probe must not count a zombie

Found while building the rewind plugin's loop toggle probe (a `■ stop` click
against a fixture whose `loop.pid` points at a `sleep` started by the probe).

`process::is_pid_alive` — the single gate for "is this loop still running?",
used by `start()`'s restart guard, `is_running`, `running_sessions`, the
orphan sweep, `get_loop` and `/loop/viewed`'s stale-`loop.pid` cleanup — was
`kill(pid, 0) == 0`. **That is also true for a zombie**: a process that has
died but whose parent has not `wait()`ed for it still holds its pid slot.
The webui can `stop()` a loop it did not start (`loop.pid` from the TUI or a
previous server instance) and nothing in this server owns that child, so
nobody reaps it on our behalf — during that window the session looked
permanently *running*: `▶ start` never came back, `/api/sessions/{id}/loop`
kept saying running, the rewind guard stayed on, and `/loop/viewed` never
cleaned the stale `loop.pid`.

`is_pid_alive` is now `kill(pid, 0) && !is_zombie(pid)`:

| platform | how the state is read | verified by |
|---|---|---|
| Linux | `/proc/<pid>/stat`, after the last `)` — `Z` (or `X`) | `stat_state_reads_the_linux_state_field` (the parse is platform-independent, so it runs everywhere) + a `cargo check --target x86_64-unknown-linux-gnu` of the branch (a full cross-check of the crate needs a cross C compiler for the dep tree, so the branch was type-checked in an isolated crate with the same code) |
| macOS | `sysctl(KERN_PROC, KERN_PROC_PID)` → `kinfo_proc`, byte 36 = `p_stat`, compared with `SZOMB` | `a_zombie_is_not_alive` / `a_live_process_is_alive` (real processes) |
| other | no answer → keep the plain signal-0 result (the pre-v0.5.58 behaviour) | — |

Two notes worth keeping:

- **`proc_pidinfo` cannot see zombies.** `PROC_PIDTBSDINFO` *and*
  `PROC_PIDT_SHORTBSDINFO` both return `0`/`ESRCH` for a zombie on Darwin 24
  (measured), so the obvious API is the wrong one here; `sysctl` is what
  `ps` itself reads.
- The macOS path depends on a **byte offset** into the kernel's frozen
  `kinfo_proc` prefix (`extern_proc`: `p_un` 16 + `p_vmspace` 8 +
  `p_sigacts` 8 + `p_flag` 4 → `p_stat` at 36). `process::tests::
  a_zombie_is_not_alive` is the canary: it spawns a child, never reaps it,
  and requires the probe to call it dead — if that offset ever stops
  meaning "state", the test fails loudly instead of silently regressing to
  "everything looks alive".

Verified: `cargo test -p rushi-web` 45 passed — "a zombie is not alive"
(spawns a child, never reaps it, asserts signal 0 still succeeds *and* the
probe calls it dead), "a live process is alive", and the Linux parse. The
browser probe drives a real `■ stop` against an unreaped fake loop and asserts
`/api/loops` drops the session (110 checks; reverting the fix fails exactly
those two checks — mutation-tested).
