use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::WebConfig;

/// Tracks running agent-loop processes by session name.
#[derive(Clone)]
pub struct LoopManager {
    cfg: Arc<WebConfig>,
    inner: Arc<RwLock<HashMap<String, u32>>>,
}

impl LoopManager {
    pub fn new(cfg: Arc<WebConfig>) -> Self {
        Self {
            cfg,
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Spawn the agent loop for `session` in its own process group so
    /// `stop` can kill the whole tree (loop + any in-flight tools).
    ///
    /// Returns the child PID.
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
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
            // New process group so we can SIGKILL the whole tree.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }

            let child = cmd
                .spawn()
                .with_context(|| format!("failed to spawn loop command {:?}", self.cfg.loop_cmd))?;
            let pid = child.id().ok_or_else(|| anyhow!("spawned process has no pid"))?;
            drop(child); // we only track the pid; the process outlives us

            map.insert(session.to_string(), pid);
            pid
        };

        info!(session, pid, "loop started");
        Ok(pid)
    }

    /// Kill the tracked process group for `session`. Returns `true` if
    /// a running loop was found and signalled, `false` if none tracked.
    pub async fn stop(&self, session: &str) -> Result<bool> {
        let pid = {
            let mut map = self.inner.write().await;
            map.remove(session)
        };

        let Some(pid) = pid else {
            return Ok(false);
        };

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
