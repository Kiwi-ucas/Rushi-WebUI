# kernelsync/ — keep rushi-webui in lockstep with the rushi kernel

The webui is a Tier-2 front-end: it shares no build-time dependency with the
kernel. Its whole contract with the kernel is:

- the append-only `events.jsonl` event vocabulary (14 types, `event.rs`),
- the `ext_status` payloads (`loop_phase`, `model_thinking`,
  `model_call_context`, `hook_applied`),
- the loop CLI it spawns (`rushi run <session> [task]`),
- the on-disk goal layout (`goal.json` + `goal-<id>.json`),
- the `rushi serve` subcommand + `[web]` config table (local bridge, not yet
  upstream).

## Files

| File | What it is |
|---|---|
| `serve-web.patch` | The WebUI bridge: `rushi serve` subcommand + `[web]` config reading, as a patch to `bin/rushi/src/main.rs`. **Based on kernel commit `b812b52`** — rebase it when the kernel moves on. Verified: `git apply --check` clean, `cargo check -p rushi` passes in a clean worktree. |
| `serve-web.orig.patch` | Record of the original uncommitted WIP diff (based on the old squashed snapshot `49e9ce2`). Contains a `unwrap_or_else(\|\| ...)` closure bug; the fixed version is `serve-web.patch`. Do not apply this one. |
| `config.local.toml` | The local working `config.toml` (model, hooks, `[tui]`, `[web]` sections). The kernel repo no longer tracks a root `config.toml`, so this is restored as an untracked file after every sync. |
| `sync.sh` | Sync workflow: `git fetch` → back up dirty state to `backup/` → `git reset --hard origin/main` → apply `serve-web.patch` → restore `config.local.toml` → rebuild kernel + webui. `--no-build` skips the builds. |
| `kernel-compat.sh` | Contract check: 14 event types present in `event.rs`, `rushi serve` present, `rushi run` CLI intact, `RewindMode = {Before, On}`, `[web]` in config. Run after every sync; exit 1 on drift. |
| `UPSTREAM_PR.md` | Payload + rationale for upstreaming the bridge to `TonyWu20/rushi`. Once merged, delete `serve-web.patch` handling from `sync.sh` (the kernel ships `serve` natively). |
| `backup/` | Per-sync snapshots of the kernel working tree (created by `sync.sh`). |

## Normal sync procedure

```sh
./kernelsync/sync.sh              # sync + rebuild
./kernelsync/kernel-compat.sh     # contract check
```

If `sync.sh` reports the patch no longer applies, rebase it:

```sh
cd ../rushi
git worktree add ../.sync-test origin/main
# edit ../.sync-test/bin/rushi/src/main.rs (re-insert the Serve bits),
git diff > ../../rushi-webui/kernelsync/serve-web.patch   # from inside the worktree
git worktree remove ../.sync-test
```

## Upstream status

PR opened 2026-09-20: <https://github.com/TonyWu20/rushi/pull/23>
(`Kiwi-ucas:rushi-serve` → `TonyWu20/rushi:main`, branch cloned at
`/tmp/rushi-upstream/rushi`). Once it merges, delete the
`serve-web.patch` step from `sync.sh` — the kernel ships `rushi serve`
natively and only `config.local.toml` restoration + rebuild remain.

Until it merges, `sync.sh` re-applies `serve-web.patch` after every
`git reset --hard origin/main`.

## Known follow-ups (webui side, not sync-related)

- Fixed 2026-09-20: `bin/rushi-web/src/main.rs` WS protocol comment now
  says the rewind `mode` is `"before|on"`, matching the kernel
  `RewindMode = {Before, On}`. (Was `before|after`.)
