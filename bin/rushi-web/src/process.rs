use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use crate::config::WebConfig;

/// Where a session's generated config lives: **beside the config it was
/// derived from**, never in the session dir.
///
/// The kernel resolves `[paths]` — `native_tool_paths` and
/// `extension_tool_paths` — against the config file's own directory
/// (`assemble`/`parse` call `resolve_tool_entry(config_dir, entry)`).
/// A generated config parked anywhere else silently resolves every tool
/// path to a missing directory, so the session assembles an empty tool
/// list, the model has nothing it may call, and it writes its tool
/// markup into the message text instead. Keeping the file in the same
/// directory makes every relative path mean exactly what it means in the
/// original. The `config.session.<id>.toml` name is covered by the
/// kernel repo's `config*.toml` ignore rule.
fn session_config_path(cfg_pin: &Path, session: &str) -> std::path::PathBuf {
    cfg_pin
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(format!("config.session.{}.toml", safe_session_id(session)))
}

/// A session id that is safe to embed in a file name next to the kernel
/// config. Session names are directory names already, so this only has
/// to defend against the odd character a name could still carry.
fn safe_session_id(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Lifecycle event for a loop process, forwarded to the session's WS
/// clients as a `loop_status` frame. `running` is a level, not a delta.
#[derive(Clone, Debug)]
pub struct LoopEvent {
    pub session: String,
    /// `true` = the loop process is alive.
    pub running: bool,
    /// Exit code on a normal exit; `None` when killed by a signal.
    pub exit: Option<i32>,
    /// `true` when the death was caused by our `stop()` (SIGKILL of the
    /// process group). Clients suppress the "stopped unexpectedly" card
    /// for intentional stops.
    pub stopped: bool,
    /// On an abnormal (non-intentional) exit: the tail of the loop's
    /// stderr (`sessions/<id>/loop.stderr`), so the UI can show *why*
    /// the loop died instead of the session going silent.
    pub detail: Option<String>,
}

/// v0.5.55 P1: the last-exit record for a session, persisted on disk at
/// `sessions/<id>/loop.last`. It is what lets a client that was closed
/// or restarted still learn which loops *finished* while it was away
/// (the green "finished-but-unviewed" lamp). Written by the per-loop
/// waiter on a normal/observed exit, and by the orphan sweep in
/// [`LoopsSnapshot::finished`] for loops that died while the server was
/// stopped (the webui then only knows the process is gone).
///
/// Deleted two ways: when a NEW loop for the session starts (a fresh
/// run supersedes the previous result — see [`LoopManager::start`]), and
/// when the user *views* the session (the `loop/viewed` endpoint), which
/// consumes the lamp.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LoopLast {
    /// The pid of the loop run that produced this record.
    pub pid: u32,
    /// Exit code on a normal exit; `None` when killed by a signal or when
    /// the record was reconstructed by the orphan sweep (the webui only
    /// knows the process is gone, not why).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// `true` when this server's `stop()` killed it.
    pub stopped: bool,
    /// Unix seconds (UTC) at which the loop exited.
    pub ts: u64,
}

/// A `LoopLast` paired with its session name — the shape the API and the
/// WS `loops` frame expose (the on-disk file has no name; the directory
/// does).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FinishedLoop {
    pub session: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    pub stopped: bool,
    pub ts: u64,
}

/// v0.5.55 P1: the whole sidebar loop state in one call.
#[derive(Serialize, Clone, Debug, Default)]
pub struct LoopsSnapshot {
    /// Sessions with a live loop process right now.
    pub running: Vec<String>,
    /// Sessions whose most recent loop has *finished* and left a
    /// `loop.last` marker, none of them currently running. Newest first.
    pub finished: Vec<FinishedLoop>,
}

/// Tracks running agent-loop processes by session name.
#[derive(Clone)]
pub struct LoopManager {
    cfg: Arc<WebConfig>,
    inner: Arc<RwLock<HashMap<String, u32>>>,
    /// Sessions whose death was initiated by `stop()`; consumed by the
    /// per-process waiter so it can flag the event as intentional.
    killed: Arc<RwLock<HashSet<String>>>,
    /// Per-session lifecycle channel. One `subscribe()` per WS client;
    /// lagging receivers drop old frames (state is a level, not a delta).
    /// `Arc` so spawned waiter tasks can move a handle into the task.
    events: Arc<broadcast::Sender<LoopEvent>>,
}

impl LoopManager {
    pub fn new(cfg: Arc<WebConfig>) -> Self {
        let (events, _) = broadcast::channel(32);
        Self {
            cfg,
            inner: Arc::new(RwLock::new(HashMap::new())),
            killed: Arc::new(RwLock::new(HashSet::new())),
            events: Arc::new(events),
        }
    }

    /// Subscribe to loop lifecycle events (all sessions; filter by
    /// `session` on the receiving side).
    pub fn subscribe(&self) -> broadcast::Receiver<LoopEvent> {
        self.events.subscribe()
    }

    /// Spawn the agent loop for `session` in its own process group so
    /// `stop` can kill the whole tree (loop + any in-flight tools).
    ///
    /// Returns the child PID. A waiter task watches the child and
    /// publishes the exit as a `LoopEvent`, so clients learn when a
    /// loop dies instead of believing an optimistic flag.
    pub async fn start(&self, session: &str) -> Result<u32> {
        let cmd0 = self.cfg.loop_cmd.first().cloned().ok_or_else(|| {
            anyhow!("loop command is empty; set [loop] command in config")
        })?;

        let pid = {
            let mut map = self.inner.write().await;
            if let Some(existing) = map.get(session) {
                if is_pid_alive(*existing) {
                    return Err(anyhow!(
                        "loop for session '{}' already running (pid {})",
                        session,
                        existing
                    ));
                }
            }

            // A stale "killed" flag from a previous stop() must not leak
            // into this run's exit event.
            self.killed.write().await.remove(session);

            let mut cmd = Command::new(&cmd0);
            cmd.args(&self.cfg.loop_cmd[1..]);
            cmd.arg(session);
            // Working directory: the session's cwd marker (the kernel's
            // `cwd`, else the webui's `.cwd`) when that directory still
            // exists, else the sessions root's parent. The kernel
            // re-anchors the session's working directory to this
            // process's cwd on every loop (re)start
            // (`refresh_session_cwd`), so this launch directory is what
            // keeps a session working inside its own project dir. The
            // config arrives through $CONFIG below, so the loop no
            // longer needs to start in the kernel checkout to find it.
            let session_cwd = crate::sessions::read_cwd_marker(
                &self.cfg.sessions_root.join(session),
            )
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir());
            let cwd = session_cwd.unwrap_or_else(|| {
                self.cfg
                    .sessions_root
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
            });
            cmd.current_dir(&cwd);
            // Pin the loop's config regardless of the working directory.
            // The kernel's resolution chain is --config → $CONFIG →
            // <exe_dir>/../config.toml → CWD/config.toml (v0.1.5,
            // crates/rushi/src/paths.rs); here the webui front-end's
            // `--config` wins, then the inherited $CONFIG, then the two
            // fallbacks. The winner is exported to the child as $CONFIG
            // so a session working directory may be ANY existing
            // directory. When nothing resolves, fail here with a clear
            // message instead of spawning a loop that dies at startup.
            let cfg_pin = {
                let inherited = std::env::var("CONFIG").ok();
                match (&self.cfg.config_path, inherited.as_deref()) {
                    (Some(p), _) if p.exists() => p.to_path_buf(),
                    (Some(p), _) => {
                        return Err(anyhow!(
                            "config file not found: {} (from --config)",
                            p.display()
                        ));
                    }
                    (None, Some(e)) if std::path::Path::new(e).exists() => {
                        std::path::PathBuf::from(e)
                    }
                    (None, Some(e)) => {
                        return Err(anyhow!("config file not found: {} (from $CONFIG)", e));
                    }
                    _ => {
                        // Fallback chain, mirroring the kernel's
                        // resolve_config_path (side-by-side, then CWD).
                        let exe = std::path::Path::new(&cmd0);
                        let side = exe
                            .parent()
                            .map(|d| d.join(".."))
                            .map(|d| d.join("config.toml"));
                        let cwd_cfg = cwd.join("config.toml");
                        let found = [side.clone(), Some(cwd_cfg.clone())]
                            .into_iter()
                            .flatten()
                            .find(|p| p.exists());
                        match found {
                            Some(p) => p,
                            None => {
                                let side_desc = side
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "(no dir)".into());
                                return Err(anyhow!(
                                    "no config.toml resolvable for the loop: \
                                     no --config, no $CONFIG, side-by-side {} missing, \
                                     {} missing; pass --config <file> to rushi-web",
                                    side_desc,
                                    cwd_cfg.display()
                                ));
                            }
                        }
                    }
                }
            };
            // Make the pin absolute: the loop resolves $CONFIG relative
            // to its own CWD (the session working directory), so a
            // relative pin would point at the wrong file.
            let cfg_pin = std::fs::canonicalize(&cfg_pin).unwrap_or(cfg_pin);

            // v0.5.44: a session may run a model entry other than the
            // config's active one. The `$MODEL` env var cannot express
            // that — only `model`/`assemble`/`compact` read it, while the
            // loop itself snapshots its context budget from `[active]` at
            // startup (the request would use B while the compaction math
            // used A). Pinning a config whose `[active]` is B is the one
            // channel the loop and every stage follow together.
            let session_dir = self.cfg.sessions_root.join(session);
            let choice = crate::sessions::read_marker(
                &session_dir,
                crate::sessions::MODEL_MARKER,
            );
            let effort = crate::sessions::read_marker(
                &session_dir,
                crate::sessions::EFFORT_MARKER,
            );
            let mut pinned = cfg_pin.clone();
            if choice.is_some() || effort.is_some() {
                match std::fs::read_to_string(&cfg_pin)
                    .map_err(|e| e.to_string())
                    .and_then(|text| {
                        crate::modelcfg::session_config(
                            &text,
                            choice.as_deref(),
                            effort.as_deref(),
                        )
                    })
                {
                    Ok(text) => {
                        let path = session_config_path(&cfg_pin, session);
                        // Sweep the v0.5.44 layout: a file left in the
                        // session dir is inert, but it reads like the
                        // config the loop is running.
                        let _ = std::fs::remove_file(
                            session_dir.join("config.session.toml"),
                        );
                        match crate::modelcfg::write_atomic(&path, &text) {
                            Ok(()) => pinned = path,
                            Err(e) => warn!(
                                session,
                                %e,
                                "cannot write the per-session config; using the shared one"
                            ),
                        }
                    }
                    Err(e) => warn!(
                        session,
                        %e,
                        "cannot derive the per-session config; using the shared one"
                    ),
                }
            }
            // Record what this launch actually runs with: the kernel logs
            // no model identity, so this marker is the only record of
            // "the model this session last used".
            if let Some(used) =
                crate::modelcfg::active_model_in(&pinned).or_else(|| choice.clone())
            {
                let _ = std::fs::write(
                    session_dir.join(crate::sessions::MODEL_USED_MARKER),
                    used,
                );
            }
            cmd.env("CONFIG", pinned.display().to_string());
            // Keys pasted in the model panel live in config.secrets.toml
            // (the kernel reads a key only from the environment). Inject
            // them under the names the entries declare; they win over an
            // inherited env var of the same name, matching what the
            // panel reports.
            for (name, value) in crate::modelcfg::load_secrets(&self.cfg) {
                cmd.env(name, value);
            }
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::null());
            // Keep stderr on disk: a loop that dies at startup (e.g. it
            // cannot find config.toml in its cwd) must be diagnosable.
            let stderr_path = self.cfg.sessions_root.join(session).join("loop.stderr");
            match std::fs::File::create(&stderr_path) {
                Ok(f) => {
                    cmd.stderr(Stdio::from(f));
                }
                Err(e) => {
                    warn!(
                        path = %stderr_path.display(),
                        %e,
                        "cannot open loop.stderr; discarding stderr"
                    );
                    cmd.stderr(Stdio::null());
                }
            }
            // New process group so we can SIGKILL the whole tree.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }

            let mut child = cmd
                .spawn()
                .with_context(|| format!("failed to spawn loop command {:?}", self.cfg.loop_cmd))?;
            let pid = child.id().ok_or_else(|| anyhow!("spawned process has no pid"))?;

            map.insert(session.to_string(), pid);

            // v0.5.55 P1: a fresh run supersedes the previous run's
            // "last exit" marker — drop it so the green "finished,
            // unviewed" lamp does not linger while this loop runs.
            let _ = std::fs::remove_file(
                self
                    .cfg
                    .sessions_root
                    .join(session)
                    .join("loop.last"),
            );

            // Publish "started" before the waiter task is even scheduled,
            // so the exit event can never overtake it in the broadcast.
            let _ = self.events.send(LoopEvent {
                session: session.to_string(),
                running: true,
                exit: None,
                stopped: false,
                detail: None,
            });

            // Watch the child; publish its exit to the WS clients.
            {
                let events = self.events.clone();
                let killed = self.killed.clone();
                let sessions_root = self.cfg.sessions_root.clone();
                let s = session.to_string();
                tokio::spawn(async move {
                    let status = child.wait().await;
                    let exit = status.ok().and_then(|st| st.code());
                    let stopped = killed.write().await.remove(&s);
                    // An abnormal exit (crash / signal, not an intentional
                    // stop) carries the stderr tail so the UI can show why
                    // the loop died instead of the session going silent.
                    let abnormal = !stopped && exit.map_or(true, |code| code != 0);
                    let detail = if abnormal {
                        stderr_tail(&sessions_root.join(&s).join("loop.stderr"), 4000)
                    } else {
                        None
                    };
                    if let Some(code) = exit {
                        info!(session = %s, pid, code, stopped, "loop exited");
                    } else {
                        warn!(session = %s, pid, stopped, "loop killed by signal");
                    }
                    let _ = events.send(LoopEvent {
                        session: s.clone(),
                        running: false,
                        exit,
                        stopped,
                        detail,
                    });
                    // v0.5.55 P1: persist the last-exit record so a client
                    // that is closed or restarted still sees the green
                    // "finished, not viewed" lamp. The file lands at
                    // sessions/<s>/loop.last and is consumed by the
                    // loop/viewed endpoint or superseded on the next start.
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let rec = LoopLast {
                        pid,
                        exit,
                        stopped,
                        ts,
                    };
                    if let Ok(json) = serde_json::to_string(&rec) {
                        let _ = std::fs::write(
                            sessions_root.join(&s).join("loop.last"),
                            json,
                        );
                    }
                });
            }

            pid
        };

        info!(session, pid, "loop started");
        Ok(pid)
    }

    /// Kill the tracked process group for `session`. Falls back to the
    /// stale `loop.pid` file (previous server instance or the TUI) when
    /// this server never started the loop. Returns `true` if a running
    /// loop was found and signalled, `false` if none tracked.
    pub async fn stop(&self, session: &str) -> Result<bool> {
        let tracked = {
            let mut map = self.inner.write().await;
            map.remove(session)
        };

        let pid = match tracked {
            Some(pid) => Some(pid),
            None => {
                let pid_file = self.cfg.sessions_root.join(session).join("loop.pid");
                std::fs::read_to_string(pid_file)
                    .ok()
                    .and_then(|t| t.trim().parse().ok())
            }
        };

        let Some(pid) = pid else {
            self.killed.write().await.remove(session);
            return Ok(false);
        };

        // Flag the death as intentional before the kill so the waiter
        // can mark the exit event.
        self.killed.write().await.insert(session.to_string());

        // SIGKILL the whole process group (negative pid).
        if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } == 0 {
            info!(session, pid, "loop process group killed");
        } else {
            let err = std::io::Error::last_os_error();
            warn!(session, pid, %err, "kill(-pid) failed (process may already be gone)");
        }
        Ok(true)
    }

    /// Whether a loop for `session` is alive — either tracked in-memory
    /// (this server started it) or, if this server never started it
    /// (a restart or the TUI left it running), via its `loop.pid` file.
    /// Mirrors `stop()`'s stale-pid fallback so the sidebar lamp and the
    /// `/loop` endpoint agree across a server restart.
    pub async fn is_running(&self, session: &str) -> bool {
        let map = self.inner.read().await;
        if let Some(&pid) = map.get(session) {
            return is_pid_alive(pid);
        }
        drop(map);
        let pid_file = self.cfg.sessions_root.join(session).join("loop.pid");
        std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
            .map_or(false, |pid| is_pid_alive(pid))
    }

    /// Names of every session with a live loop process — the
    /// server-side truth for the clients' sidebar lamps, sent as a
    /// `loops` frame on every WS connect so a fresh browser can light
    /// the breathing lamps of sessions it has not seen start.
    ///
    /// Combines the in-memory set (loops this server started) with a
    /// rescan of each session's `loop.pid`, so loops left running by a
    /// previous server instance or the TUI still light their lamps.
    pub async fn running_sessions(&self) -> Vec<String> {
        let mut out: Vec<String> = {
            let map = self.inner.read().await;
            map.iter()
                .filter(|&(_, &pid)| is_pid_alive(pid))
                .map(|(s, _)| s.clone())
                .collect()
        };
        if let Ok(dirs) = std::fs::read_dir(&self.cfg.sessions_root) {
            for entry in dirs.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if out.iter().any(|s| s == &name) {
                    continue;
                }
                let pid_file = entry.path().join("loop.pid");
                if let Ok(t) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = t.trim().parse::<u32>() {
                        if is_pid_alive(pid) {
                            out.push(name);
                        }
                    }
                }
            }
        }
        out
    }

    /// v0.5.55 P1: the whole sidebar loop state in one call.
    ///
    /// `running` delegates to [`LoopManager::running_sessions`]. `finished`
    /// is every session with a `loop.last` marker that is NOT currently
    /// running — i.e. loops that finished while a client may have been
    /// away. Before reading the markers, an **orphan sweep** fills in a
    /// best-effort marker for any session whose `loop.pid` is dead but has
    /// no `loop.last` yet (a loop that died while this server was stopped,
    /// or that the TUI started and this webui never observed). This is what
    /// lets the green "finished, unviewed" lamp catch up within one poll.
    pub async fn loops_snapshot(&self) -> LoopsSnapshot {
        let running = self.running_sessions().await;
        let root = &self.cfg.sessions_root;
        let mut finished: Vec<FinishedLoop> = Vec::new();
        if let Ok(dirs) = std::fs::read_dir(root) {
            for entry in dirs.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                // A running session's lamp is the orange one, not green.
                if running.iter().any(|s| s == &name) {
                    continue;
                }
                let marker = path.join("loop.last");
                // Orphan sweep: a dead `loop.pid` with no marker means the
                // loop finished outside this server's observation. Record
                // it (unknown exit, not an intentional stop).
                if !marker.exists() {
                    let pid_file = path.join("loop.pid");
                    if let Ok(t) = std::fs::read_to_string(&pid_file) {
                        if let Ok(pid) = t.trim().parse::<u32>() {
                            if !is_pid_alive(pid) {
                                let ts = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0);
                                let rec = LoopLast {
                                    pid,
                                    exit: None,
                                    stopped: false,
                                    ts,
                                };
                                if let Ok(json) = serde_json::to_string(&rec) {
                                    let _ = std::fs::write(&marker, json);
                                }
                            }
                        }
                    }
                }
                if let Ok(txt) = std::fs::read_to_string(&marker) {
                    if let Ok(rec) = serde_json::from_str::<LoopLast>(&txt) {
                        finished.push(FinishedLoop {
                            session: name,
                            pid: rec.pid,
                            exit: rec.exit,
                            stopped: rec.stopped,
                            ts: rec.ts,
                        });
                    }
                }
            }
        }
        // Newest first so a client can render in chronological order.
        finished.sort_by(|a, b| b.ts.cmp(&a.ts));
        LoopsSnapshot { running, finished }
    }
}

/// Signal-0 liveness probe. Public so the loop-state endpoint can
/// check a `loop.pid` left by a previous server instance or the TUI.
pub fn is_pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Read the last `max_bytes` of a file (the loop's `loop.stderr`
/// capture), trimmed. Returns `None` when the file is missing or has
/// no content. Used to explain abnormal loop exits in the UI.
fn stderr_tail(path: &Path, max_bytes: usize) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let mut start = data.len().saturating_sub(max_bytes);
    // Advance past a UTF-8 continuation byte (0b10xxxxxx) so the slice
    // never splits a character.
    while start < data.len() && data[start] & 0xC0 == 0x80 {
        start += 1;
    }
    let text = String::from_utf8_lossy(&data[start..]).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel resolves `[paths]` tool entries against the config
    /// file's directory, so the per-session config has to land in the
    /// SAME directory as the config it derives from. Regression guard
    /// for v0.5.44, where the generated file went into the session dir
    /// and every session with a model/effort override silently ran with
    /// an empty tool list.
    #[test]
    fn session_config_lands_beside_the_config_it_derives_from() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let path = session_config_path(&cfg, "my session/../x");
        assert_eq!(path.parent(), Some(dir.path()));
        // No separator survives sanitizing: the file stays in that dir.
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "config.session.my_session_.._x.toml"
        );
        // Same directory in, same tool paths out.
        assert_eq!(
            path.parent().unwrap().join("tools/bash"),
            dir.path().join("tools/bash"),
        );
    }

    #[test]
    fn session_config_path_falls_back_to_the_cwd_for_a_bare_name() {
        // No parent directory: join onto "" (i.e. the relative name),
        // never panic.
        let path = session_config_path(Path::new("config.toml"), "s1");
        assert_eq!(path, Path::new("config.session.s1.toml"));
    }
}
