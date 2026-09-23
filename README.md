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
`approval`, `rewind` (fork divider), `user_message_retract`
(strikethrough).

## Goal panel (web-native goal-ext)

The TUI's `goal-ext` row is mirrored as a sidebar panel that reads
and writes the same on-disk layout the kernel hooks use
(`goal.json` pointer + `goal-<id>.json` per-goal files, via the
`goal-state` crate's schema): create / pause / resume / clear / edit
from the browser. The kernel's `model.before` / `run.idle` goal
hooks keep working unchanged.

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
(`web/dist/index.html`), so `rushi-web` is one static binary. After
editing the frontend, touch any `src/*.rs` (or re-run the launcher's
build) to re-embed. In debug builds rust-embed reads `web-leptos/dist/`
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

## Dev notes

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
- No build step for the frontend: `web/dist/index.html` is the
  source of truth and the embedded artifact.
