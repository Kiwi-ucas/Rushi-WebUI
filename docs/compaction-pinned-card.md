# Pinned "context summary" card (v0.5.57)

The session's latest `compaction_summary` is pinned as a collapsible card at
the top of the transcript, so it stays visible across a page reload.

## Why

A `compaction_summary` event **is** persisted — it is written to
`sessions/<id>/events.jsonl` like any other event, and the server serves the
whole log unfiltered (`GET /api/sessions/{id}/events`). The reason it
"disappears" after a refresh is **windowing, not storage**:

- On (re)connect, the WS `history` frame ships only the **last `HIST_PAGE`
  (200)** events (`main.rs`), and that is what lands in `state.events`.
- A compaction usually happened far earlier than 200 events, so its card is
  not in the initial window — it is buried in the scrollback, reachable only
  by paging "load earlier" many times.
- Live (no refresh) the full event list accumulates in the DOM, so the card
  is visible right after compaction; a refresh resets the window to the last
  200 and the card is gone from view.

So the symptom "只在 compact 时能看到，刷新后就看不到了" is a display-window
issue. The fix surfaces the **latest** summary (the current context handoff —
the most useful thing to see when you return to a session) without dragging
thousands of old events into the window.

## Design

Pin the **latest** `compaction_summary` as a dedicated collapsible card,
rather than auto-paging the whole log back to it (a deep log would mean many
pages of 200 on every open — slow and it scrolls the user to old content).

- The pinned card is **in addition to** the normal event cards: the original
  `compaction_summary` event cards remain in the scrollback and still render
  as ordinary events when they fall inside a loaded window.
- Collapsed by default (one-line header) so it is persistent but not
  intrusive; the body is the same markdown renderer an assistant message uses
  (`md_blocks_view`), in its own 40%-height scroller.

## Endpoints / pieces

| Piece | Where | What |
|---|---|---|
| `SessionStore::latest_compaction(id)` | `bin/rushi-web/src/sessions.rs` | tail-scan `events.jsonl` backwards, return the most recent `compaction_summary` event (stops at first hit → cheap). `None` when the log is missing/empty or has no compaction. |
| `GET /api/sessions/{id}/compaction` | `bin/rushi-web/src/main.rs` `get_compaction` | 200 + the event, or 404 + `null` when there is none. |
| `api::load_latest_compaction(id)` | `web-leptos/src/api.rs` | returns `Option<CompactionSummary>` (None on 4xx / empty summary). |
| `CompactionSummary { summary, ts }` + `AppState.latest_compaction` | `web-leptos/src/model.rs` | the pinned card's data + the reactive signal. |
| fetch on session select | `web-leptos/src/ui.rs` `select_session` | clears the signal on switch, refetches the new session's latest summary. |
| `PinnedCompactionCard` | `web-leptos/src/transcript.rs` | renders at the top of `#transcript`; **no `.event` class** so the pile engine's 1:1 `.event`→index mapping is unaffected. |
| `.comp-card*` | `web-leptos/style.css` | raised card (`--shadow`) with an accent left rim to read as "pinned/current". |

## Verification

- `GET /api/sessions/Webui/compaction` → 200, `type=compaction_summary`,
  `~14 KB` summary. `GET /api/sessions/essence/compaction` (a session with no
  compaction) → 404 `null`.
- Served on `:8480`: `index.html` references the new asset hashes; the served
  CSS contains the 7 `.comp-card*` rules; the served wasm contains the
  `/compaction` + "context summary" markers.
