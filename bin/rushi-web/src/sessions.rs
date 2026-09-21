use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::Serialize;
use tokio::fs;

use crate::config::WebConfig;

/// Manages session directories and their `events.jsonl` logs.
#[derive(Clone)]
pub struct SessionManager {
    cfg: Arc<WebConfig>,
}

impl SessionManager {
    pub fn new(cfg: Arc<WebConfig>) -> Self {
        Self { cfg }
    }

    /// The directory that holds one sub-directory per session.
    pub fn sessions_root(&self) -> &Path {
        &self.cfg.sessions_root
    }

    /// Path to a specific session directory.
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.cfg.sessions_root.join(id)
    }

    /// Path to a specific session's event log.
    pub fn events_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("events.jsonl")
    }

    // ── listing ───────────────────────────────────────────────────

    /// List all sessions that have an `events.jsonl`, newest-first.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let root = self.sessions_root().to_path_buf();
        let mut out = Vec::new();

        let mut entries = match fs::read_dir(&root).await {
            Ok(e) => e,
            Err(_) => return out, // no sessions dir yet → empty list
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let ev_path = dir.join("events.jsonl");
            let exists = ev_path.exists();
            let last_modified = fs::metadata(&ev_path)
                .await
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| {
                    let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                    Some(secs.as_secs())
                });

            out.push(SessionInfo {
                name,
                has_events: exists,
                last_modified,
            });
        }

        out.sort_by(|a, b| {
            b.last_modified
                .unwrap_or(0)
                .cmp(&a.last_modified.unwrap_or(0))
        });
        out
    }

    // ── event access ──────────────────────────────────────────────

    /// Read the full event log as a JSON array of raw event objects.
    pub async fn events(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let path = self.events_path(id);
        if !path.exists() {
            return Err(anyhow!("session '{}' not found", id));
        }
        let text = fs::read_to_string(&path).await?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|_| serde_json::Value::String(line.to_string()));
            out.push(v);
        }
        Ok(out)
    }

    // ── lifecycle: rename / delete ────────────────────────────────

    /// Rename a session directory. The directory may not exist yet
    /// (newly-created sessions start empty) — renaming is then a no-op
    /// unless the target collides.
    pub async fn rename(&self, old: &str, new: &str) -> Result<()> {
        validate_session_name(new)?;
        if old == new {
            return Ok(());
        }
        let root = self.sessions_root();
        let from = root.join(old);
        let to = root.join(new);
        if to.exists() {
            return Err(anyhow!("session '{}' already exists", new));
        }
        if !from.is_dir() {
            return Err(anyhow!("session '{}' not found", old));
        }
        fs::rename(&from, &to).await?;
        Ok(())
    }

    /// Delete a session directory and all its files (events, goal
    /// state, tools log, loop.pid).
    pub async fn delete(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        if !dir.is_dir() {
            return Err(anyhow!("session '{}' not found", id));
        }
        fs::remove_dir_all(&dir).await?;
        Ok(())
    }
}

/// Validate a session name: a single path component, no `.`/`..`.
fn validate_session_name(name: &str) -> Result<()> {
    let n = name.trim();
    if n.is_empty() || n == "." || n == ".." {
        return Err(anyhow!("invalid session name"));
    }
    if n.contains('/') || n.contains('\0') {
        return Err(anyhow!("invalid session name: {name:?}"));
    }
    if n.starts_with('-') {
        return Err(anyhow!("session name must not start with '-'"));
    }
    Ok(())
}

impl SessionManager {
    /// Create the session directory and an empty `events.jsonl` if
    /// missing.
    async fn ensure_session(&self, id: &str) -> Result<()> {
        let dir = self.cfg.sessions_root.join(id);
        fs::create_dir_all(&dir).await?;
        let ev = dir.join("events.jsonl");
        if !ev.exists() {
            fs::File::create(&ev).await?;
        }
        Ok(())
    }

    /// Path to a session's working-directory marker file.
    fn cwd_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join(".cwd")
    }

    /// The session's configured working directory (from `.cwd`), if it
    /// is set and still an existing directory.
    pub async fn cwd(&self, id: &str) -> Option<PathBuf> {
        let text = fs::read_to_string(self.cwd_path(id)).await.ok()?;
        let p = PathBuf::from(text.trim());
        if p.is_dir() {
            Some(p)
        } else {
            None
        }
    }

    /// Create a session and optionally record its working directory.
    /// `cwd`, when non-empty, must be an existing directory.
    pub async fn create(&self, id: &str, cwd: Option<&str>) -> Result<()> {
        validate_session_name(id)?;
        self.ensure_session(id).await?;
        if let Some(cwd) = cwd {
            let c = cwd.trim();
            if !c.is_empty() {
                let p = PathBuf::from(c);
                if !p.is_dir() {
                    return Err(anyhow!("working directory does not exist: {c}"));
                }
                let canon = p.canonicalize().unwrap_or(p);
                fs::write(self.cwd_path(id), canon.to_string_lossy().as_bytes()).await?;
            }
        }
        Ok(())
    }

    // ── appends ───────────────────────────────────────────────────

    /// Append a `user_message` event. Returns the 1-based line number.
    pub async fn append_user_message(
        &self,
        id: &str,
        content: &str,
        queue: Option<&str>,
    ) -> Result<u64> {
        self.ensure_session(id).await?;

        let mut obj = serde_json::json!({
            "v": 1u8,
            "type": "user_message",
            "ts": now_rfc3339(),
            "content": content,
        });
        if let Some(q) = queue {
            obj["queue"] = serde_json::Value::String(q.to_string());
        }

        let line = serde_json::to_string(&obj)?;
        self.append_line(id, &line)
    }

    /// Append an `approval` event.
    pub async fn append_approval(&self, id: &str, req_id: &str, decision: &str) -> Result<()> {
        self.ensure_session(id).await?;
        let obj = serde_json::json!({
            "v": 1u8,
            "type": "approval",
            "ts": now_rfc3339(),
            "id": req_id,
            "decision": decision,
        });
        let line = serde_json::to_string(&obj)?;
        self.append_line(id, &line)?;
        Ok(())
    }

    /// Append a `rewind` event. `mode` is "before" or "on".
    pub async fn append_rewind(&self, id: &str, target_seq: u64, mode: &str) -> Result<()> {
        self.ensure_session(id).await?;
        let obj = serde_json::json!({
            "v": 1u8,
            "type": "rewind",
            "ts": now_rfc3339(),
            "target_seq": target_seq,
            "mode": mode,
        });
        let line = serde_json::to_string(&obj)?;
        self.append_line(id, &line)?;
        Ok(())
    }

    /// Append one JSON line to `events.jsonl`. Mirrors the kernel's
    /// `LogLine::commit` semantics: exclusive flock + one write(2).
    ///
    /// Uses blocking `std::fs` (one small local write) so the
    /// write-then-flock ordering matches `LogLine::commit` exactly.
    fn append_line(&self, id: &str, json_line: &str) -> Result<u64> {
        let path = self.events_path(id);

        // 1-based line number of the line we are about to write.
        let current_lines = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .count() as u64;

        let mut bytes = Vec::with_capacity(json_line.len() + 1);
        bytes.extend_from_slice(json_line.as_bytes());
        bytes.push(b'\n');

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let fd = file.as_raw_fd();
        if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
            file.write_all(&bytes)?;
            let _ = unsafe { libc::flock(fd, libc::LOCK_UN) };
        } else {
            file.write_all(&bytes)?;
        }

        Ok(current_lines + 1)
    }
}

/// A summary entry returned by `GET /api/sessions`.
#[derive(Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub has_events: bool,
    /// Unix timestamp (seconds) of the last modification, if any.
    pub last_modified: Option<u64>,
}

/// Tail `path` for new lines and forward them over `tx`.
///
/// Polls the file every 50 ms; only complete (newline-terminated)
/// lines are emitted. If the file shrinks (truncated/rotated) the
/// read offset resets to 0. Exits when the channel receiver is
/// dropped (the WebSocket connection went away).
pub async fn tail_file(path: PathBuf, tx: tokio::sync::mpsc::Sender<String>) {
    let mut offset: u64 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut partial: Vec<u8> = Vec::new();

    loop {
        // Read bytes from `offset` to end.
        let new_bytes: Vec<u8> = std::fs::File::open(&path)
            .ok()
            .and_then(|mut f| {
                let mut b = Vec::new();
                if f.seek(SeekFrom::Start(offset)).is_err() {
                    return None;
                }
                if f.read_to_end(&mut b).is_err() {
                    return None;
                }
                Some(b)
            })
            .unwrap_or_default();

        // Detect truncation/rotation.
        if let Ok(m) = std::fs::metadata(&path) {
            if m.len() < offset {
                offset = 0;
                partial.clear();
            }
        }

        let mut buf = std::mem::take(&mut partial);
        buf.extend_from_slice(&new_bytes);
        let mut consumed: u64 = 0;
        let mut rest = buf.as_slice();
        loop {
            match rest.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let line: String = String::from_utf8_lossy(&rest[..i]).into_owned();
                    rest = &rest[i + 1..];
                    consumed += i as u64 + 1;
                    if !line.trim().is_empty()
                        && tx.send(line).await.is_err()
                    {
                        return; // receiver gone
                    }
                }
                None => {
                    partial = rest.to_vec();
                    break;
                }
            }
        }
        offset += consumed;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Tail the session-local model stream channel (`.model-stream`),
/// which the model subprocess writes live SSE deltas into while a
/// model call is in flight. The loop owns the file's lifecycle: it
/// is created before each call and deleted after, so the file is
/// absent most of the time.
///
/// Starts at the file's current end (a late connection must not
/// replay an in-progress message's deltas; the final
/// `assistant_message` event carries the full text anyway). A
/// truncate (the next model call opens with `File::create`) resets
/// the offset so the new call's deltas flow. Polls every 100 ms.
pub async fn tail_model_stream(path: PathBuf, tx: tokio::sync::mpsc::Sender<String>) {
    let mut offset: u64 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut partial: Vec<u8> = Vec::new();

    loop {
        match std::fs::metadata(&path) {
            // No file: idle between model calls. A stale file left by
            // a killed call is inert; the next call truncates it, which
            // the truncate branch below detects.
            Err(_) => {
                offset = 0;
                partial.clear();
            }
            Ok(m) => {
                if m.len() < offset {
                    offset = 0;
                    partial.clear();
                }
                if let Ok(new_bytes) = std::fs::File::open(&path).and_then(|mut f| {
                    if f.seek(SeekFrom::Start(offset)).is_err() {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut b = Vec::new();
                    f.read_to_end(&mut b)?;
                    Ok(b)
                }) {
                    let mut buf = std::mem::take(&mut partial);
                    buf.extend_from_slice(&new_bytes);
                    let mut consumed: u64 = 0;
                    let mut rest = buf.as_slice();
                    loop {
                        match rest.iter().position(|&b| b == b'\n') {
                            Some(i) => {
                                let line: String = String::from_utf8_lossy(&rest[..i]).into_owned();
                                rest = &rest[i + 1..];
                                consumed += i as u64 + 1;
                                if !line.trim().is_empty()
                                    && tx.send(line).await.is_err()
                                {
                                    return; // receiver gone
                                }
                            }
                            None => {
                                partial = rest.to_vec();
                                break;
                            }
                        }
                    }
                    offset += consumed;
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// RFC 3339 UTC timestamp with seconds precision, matching the
/// kernel's `ts` format (e.g. `2026-09-15T09:36:21Z`).
fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
