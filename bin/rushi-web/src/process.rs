use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use crate::config::WebConfig;

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
            // Working directory: the session's own `.cwd` marker if the
            // new-session dialog recorded one, else the default — the
            // sessions root's parent (the kernel checkout), which the
            // loop binary needs to resolve config.toml and its sibling
            // stage binaries.
            let session_cwd = std::fs::read_to_string(
                self.cfg.sessions_root.join(session).join(".cwd"),
            )
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir());
            let cwd = session_cwd.unwrap_or_else(|| {
                self.cfg
                    .sessions_root
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
            });
            cmd.current_dir(cwd);
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

            // Publish "started" before the waiter task is even scheduled,
            // so the exit event can never overtake it in the broadcast.
            let _ = self.events.send(LoopEvent {
                session: session.to_string(),
                running: true,
                exit: None,
                stopped: false,
            });

            // Watch the child; publish its exit to the WS clients.
            {
                let events = self.events.clone();
                let killed = self.killed.clone();
                let s = session.to_string();
                tokio::spawn(async move {
                    let status = child.wait().await;
                    let exit = status.ok().and_then(|st| st.code());
                    let stopped = killed.write().await.remove(&s);
                    if let Some(code) = exit {
                        info!(session = %s, pid, code, stopped, "loop exited");
                    } else {
                        warn!(session = %s, pid, stopped, "loop killed by signal");
                    }
                    let _ = events.send(LoopEvent {
                        session: s,
                        running: false,
                        exit,
                        stopped,
                    });
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

    /// Whether a tracked loop for `session` is still alive.
    pub async fn is_running(&self, session: &str) -> bool {
        let map = self.inner.read().await;
        map.get(session).map(|&pid| is_pid_alive(pid)).unwrap_or(false)
    }
}

/// Signal-0 liveness probe. Public so the loop-state endpoint can
/// check a `loop.pid` left by a previous server instance or the TUI.
pub fn is_pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
