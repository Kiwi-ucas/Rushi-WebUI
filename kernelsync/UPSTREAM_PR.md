# Upstream PR: `rushi serve` subcommand + `[web]` config table

Target repo: `TonyWu20/rushi` (the kernel).
Payload: `serve-web.patch` (single file, `bin/rushi/src/main.rs`, based on `b812b52`).

## Rationale

`rushi-web` is a Tier-2 front-end, exactly like the TUI. The kernel already
ships a `rushi tui` subcommand that resolves the TUI binary
(`[tui].binary` → side-by-side → `PATH`) and spawns it with the same
`--config` and `--session` the TUI launcher would use. `rushi serve` is the
mirror for the WebUI: same resolution order over a `[web].binary` config
key, forwarding `--sessions-root` (from `[paths].sessions_root`),
`--host`/`--port` (from `[web]`), and `--loop-cmd "<exe> run"` so the web
server spawns the same loop the TUI would.

No new dependencies: reuses the `toml` parse already used by
`config_tui_binary`; `std::process::Command` already in the file.

## What the patch adds

1. `Command::Serve` variant (enum, with doc comment).
2. `Command::Serve => { ... }` match arm: resolve the web binary, forward
   args, `child.status()`, exit with the child's code.
3. Four helpers: `resolve_web_binary`, `config_web_binary`,
   `config_sessions_root`, `config_web_key_str`.

## Verification done

- Applies cleanly to `b812b52:bin/rushi/src/main.rs` (`git apply --check`).
- `cargo check -p rushi` passes in a clean worktree of `b812b52` + patch
  (+ a `config.toml` with a `[web]` table).
- One fix versus the original WIP: `std::env::current_exe().unwrap_or_else`
  takes a 1-arg closure (`|_|`) — the original WIP had a zero-arg closure
  that would not compile.

## Suggested config-docs addition (separate, optional)

In the `docs/reference` config reference, document:

```toml
# [web]
# binary = "rushi-web"   # WebUI binary the `rushi serve` subcommand spawns
# host = "127.0.0.1"     # forwarded to rushi-web --host
# port = 8480            # forwarded to rushi-web --port
```

And add a `rushi serve` row to the README subcommands table:

```
| `rushi serve` | Launch the WebUI front-end (`rushi-web`), if one is installed |
```
