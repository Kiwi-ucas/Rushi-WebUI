# rushi-webui

Web front-end for the rushi agent harness. Same Tier-2 position as
`rushi-tui`: a replaceable view over the same event-sourced session
log. The kernel, tools, and hooks are untouched.

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
  [--ext-dir DIR ...]
```

`--ext-dir` is accepted for forward compatibility (CLI extension
proxying, Phase-3 option A); the web-native equivalents (goal panel +
status strip) are what's wired today.

## Single-binary distribution

The SPA is embedded at compile time with `rust-embed`
(`web/dist/index.html`), so `rushi-web` is one static binary. After
editing the frontend, touch any `src/*.rs` (or re-run the launcher's
build) to re-embed.

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
| WS | `/ws/sessions/{id}` | history frame + live event frames; inbound `message`/`approval`/`rewind`/`start`/`stop` frames |

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
