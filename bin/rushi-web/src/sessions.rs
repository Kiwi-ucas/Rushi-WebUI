use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::Serialize;
use tokio::fs;

use crate::config::WebConfig;

/// A page of events returned by `events_windowed`.
///
/// `oldest_line` is the 1-based line number (counting only non-empty lines)
/// of the first event in this page.  `oldest_line == 1` means the very
/// beginning of the log; `has_more` is false when no older lines remain.
#[derive(Clone, Debug, Serialize)]
pub struct EventsPage {
    /// Events in chronological order (oldest first within this page).
    pub events: Vec<serde_json::Value>,
    /// 1-based line number of the first event in this page.
    pub oldest_line: u64,
    /// Total number of non-empty lines in the events log.
    pub total_lines: u64,
    /// Whether older events exist above `oldest_line`.
    pub has_more: bool,
    /// Total rounds across the FULL log (1 + user_message line count),
    /// so the client can label round chips with global numbers even
    /// while only a window of the log is loaded.
    pub total_rounds: u64,
}

/// v0.5.44: the model entry the user picked for this session (absent =
/// follow the config's active entry).
pub const MODEL_MARKER: &str = ".model";
/// v0.5.44: the entry the webui actually pinned when it launched the last
/// loop — the only record of "what this session last ran with", since the
/// kernel writes no model identity into the event log.
pub const MODEL_USED_MARKER: &str = ".model.used";
/// v0.5.45: this session's reasoning effort, overriding the entry's own
/// (the kernel keeps effort per entry, so the webui pins it in the
/// session config it generates at launch).
pub const EFFORT_MARKER: &str = ".effort";

/// Read a one-line marker file, trimmed; `None` when absent or empty.
pub fn read_marker(session_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(session_dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The session's recorded working directory, as a plain string.
///
/// The kernel writes `<session>/cwd` (no dot) and rewrites it on every
/// loop (re)start from the loop process's own cwd (v0.1.5,
/// `bin/rushi/src/run_loop.rs::refresh_session_cwd`). The webui records
/// the intended directory under `.cwd` when it creates a session, so a
/// session that never ran a loop carries only the dotted name. Read the
/// kernel's marker first and fall back to ours.
pub fn read_cwd_marker(session_dir: &Path) -> Option<String> {
    read_marker(session_dir, "cwd").or_else(|| read_marker(session_dir, ".cwd"))
}

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
        // Read once for every session's fallback rather than per row.
        let active_model = crate::modelcfg::active_model(&self.cfg);

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

            // M7: the session's working directory, read as a plain
            // string — the dispatch view groups by project. A
            // missing/stale marker is fine (None), unlike cwd() which
            // also validates the dir still exists.
            let cwd = read_cwd_marker(&dir);
            // v0.5.44: what this session runs with. `.model.used` is what
            // the webui pinned at the last launch (the kernel logs no
            // model identity, so this is the only record), `.model` is
            // the pending choice for the next launch.
            // The pending choice first: while a loop runs it is what the
            // loop was pinned to (the chip is disabled mid-run, so the
            // two cannot disagree), and while idle it is what the next
            // launch will use. `.model.used` then reports the entry the
            // LAST launch used, which can differ from the global active
            // if that changed afterwards.
            let model = read_marker(&dir, MODEL_MARKER)
                .or_else(|| read_marker(&dir, MODEL_USED_MARKER))
                .or_else(|| active_model.clone());

            out.push(SessionInfo {
                name,
                has_events: exists,
                last_modified,
                created: Self::created_ts(&dir, &ev_path).await,
                cwd,
                model,
                effort: read_marker(&dir, EFFORT_MARKER),
            });
        }

        out.sort_by(|a, b| {
            b.last_modified
                .unwrap_or(0)
                .cmp(&a.last_modified.unwrap_or(0))
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// v0.5.30: a session's creation timestamp in unix seconds.
    /// Prefers the `.created` marker written by `ensure_session`;
    /// falls back to the first event's `ts` for pre-marker sessions
    /// (including ones created by the CLI, which never writes the
    /// marker). `None` when the session has no events at all.
    async fn created_ts(dir: &Path, ev_path: &Path) -> Option<u64> {
        if let Ok(t) = fs::read_to_string(dir.join(".created")).await {
            if let Ok(v) = t.trim().parse::<u64>() {
                return Some(v);
            }
        }
        Self::first_line_ts(ev_path).await
    }

    /// Read only the FIRST line of an events file (never the whole
    /// log — these can be multi-MB) and return its `ts` as unix
    /// seconds.
    async fn first_line_ts(ev_path: &Path) -> Option<u64> {
        use tokio::io::AsyncBufReadExt;
        let file = fs::File::open(ev_path).await.ok()?;
        let mut reader = tokio::io::BufReader::new(file);
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        let ts = v.get("ts")?.as_str()?;
        chrono::DateTime::parse_from_rfc3339(ts).ok()?.timestamp().try_into().ok()
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

    /// Return the session's MOST RECENT `compaction_summary` event (the
    /// current context-handoff summary), by scanning the event log
    /// backwards from the end.  `None` when the log is missing, empty,
    /// or has no compaction_summary yet.  Only the tail is scanned: we
    /// stop at the first hit, so this stays cheap for long logs.
    pub async fn latest_compaction(&self, id: &str) -> Option<serde_json::Value> {
        let path = self.events_path(id);
        let data = match fs::read_to_string(&path).await {
            Ok(d) if !d.trim().is_empty() => d,
            _ => return None,
        };
        let lines: Vec<&str> = data.lines().filter(|l| !l.trim().is_empty()).collect();
        for line in lines.iter().rev() {
            // cheap pre-filter before the (heavier) JSON parse
            if !line.contains("\"compaction_summary\"") {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("compaction_summary") {
                    return Some(v);
                }
            }
        }
        None
    }

    /// Read a window of events from the log, for truncated history loading.
    ///
    /// `before_line` is the 1-based line number of the oldest event the
    /// client already has.  The method returns up to `limit` events
    /// immediately before that line.  When `before_line` is `None` the
    /// last `limit` events are returned.
    ///
    /// Only non-empty lines are counted (matching `events()` semantics).
    /// JSON parsing is applied only to the returned window, not the
    /// whole file, so this stays fast even for very large logs.
    pub async fn events_windowed(
        &self,
        id: &str,
        before_line: Option<u64>,
        limit: u64,
    ) -> Result<EventsPage> {
        let path = self.events_path(id);
        if !path.exists() {
            return Err(anyhow!("session '{}' not found", id));
        }
        let text = fs::read_to_string(&path).await?;
        // Collect non-empty line slices (no JSON parse yet — cheap).
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let total = lines.len() as u64;

        let limit = limit.min(1000); // cap a single page

        // end = exclusive upper bound (0-based) of the window.
        let end: u64 = match before_line {
            Some(b) => b.saturating_sub(1).min(total),
            None => total,
        };
        let start = end.saturating_sub(limit);

        // Rounds across the FULL log, matching the client's
        // compute_rounds: the first event opens round 1, and each LATER
        // user_message line opens a new round. So total_rounds =
        // 1 + (user_message lines that are not the first event).
        // Quoted-type string scan (no JSON parse — cheap on multi-MB
        // logs); "user_message_retract" lines do NOT match, since their
        // type string is "user_message_retract".
        let user_msgs = lines.iter().filter(|l| l.contains(r#""user_message""#)).count();
        let first_is_um = lines.first().is_some_and(|l| l.contains(r#""user_message""#));
        let later_ums = user_msgs.saturating_sub(if first_is_um { 1 } else { 0 });
        let total_rounds = if total == 0 { 0 } else { 1 + later_ums as u64 };

        let events: Vec<serde_json::Value> = lines[start as usize..end as usize]
            .iter()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|_| serde_json::Value::String((*line).to_string()))
            })
            .collect();

        let oldest_line = start + 1; // 1-based
        Ok(EventsPage {
            events,
            oldest_line,
            total_lines: total,
            has_more: start > 0,
            total_rounds,
        })
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
        // v0.5.30: creation-time marker (unix seconds). Written only
        // when missing, so a session that already exists (created by
        // the CLI or by an earlier server instance) keeps its original
        // creation stamp — the marker records first sight.
        let marker = dir.join(".created");
        if !marker.exists() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = fs::write(&marker, secs.to_string()).await;
        }
        let ev = dir.join("events.jsonl");
        if !ev.exists() {
            fs::File::create(&ev).await?;
        }
        Ok(())
    }

    /// Path to the webui's own working-directory marker, written when a
    /// session is created. The kernel keeps its own `cwd` marker next to
    /// it; read the session workdir through [`read_cwd_marker`] instead
    /// of this path.
    pub fn cwd_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join(".cwd")
    }

    /// The session's recorded working directory as a plain string:
    /// the kernel's `cwd` marker, else the webui's `.cwd`.
    pub fn cwd_marker(&self, id: &str) -> Option<String> {
        read_cwd_marker(&self.session_dir(id))
    }

    /// v0.5.44: the model entry this session was told to run with, or
    /// `None` to follow the config's active entry.
    pub fn model_choice(&self, id: &str) -> Option<String> {
        read_marker(&self.session_dir(id), MODEL_MARKER)
    }

    /// v0.5.45: record (or, with `None`, clear) this session's reasoning
    /// effort. Applied in the session config the loop is pinned to, so it
    /// takes effect the next time the loop starts.
    pub async fn set_effort(&self, id: &str, effort: Option<&str>) -> Result<()> {
        validate_session_name(id)?;
        self.ensure_session(id).await?;
        let path = self.session_dir(id).join(EFFORT_MARKER);
        match effort.map(str::trim).filter(|s| !s.is_empty()) {
            Some(e) => fs::write(&path, e.as_bytes()).await?,
            None => {
                let _ = fs::remove_file(&path).await;
            }
        }
        Ok(())
    }

    /// Set (or, with `None`, clear) this session's model entry. The
    /// choice takes effect the next time the session's loop starts: the
    /// loop pins a config whose `[active]` is this entry, so the loop and
    /// every stage binary agree (see `modelcfg::session_config_with_active`).
    pub async fn set_model(&self, id: &str, model: Option<&str>) -> Result<()> {
        validate_session_name(id)?;
        self.ensure_session(id).await?;
        let path = self.session_dir(id).join(MODEL_MARKER);
        match model.map(str::trim).filter(|s| !s.is_empty()) {
            Some(m) => fs::write(&path, m.as_bytes()).await?,
            None => {
                let _ = fs::remove_file(&path).await;
                // "Follow the global entry again" also drops the record of
                // the last run, so the card shows the entry the next
                // launch will actually use.
                let _ = fs::remove_file(self.session_dir(id).join(MODEL_USED_MARKER)).await;
            }
        }
        Ok(())
    }

    /// v0.5.56: whether time-injection is ON for this session — the
    /// sidebar `time` plugin's state. The contract (marker name, off
    /// values, the default-on rule) lives in [`crate::time_inject`], the
    /// same module the hook mirrors, so this is only the file I/O.
    pub fn time_inject_enabled(&self, id: &str) -> bool {
        crate::time_inject::enabled_from_marker(read_marker(
            &self.session_dir(id),
            crate::time_inject::MARKER,
        )
        .as_deref())
    }

    /// v0.5.56: record the time-inject toggle. `true` clears the marker
    /// (back to the default-on state); `false` writes the explicit off
    /// marker the hook reads. The hook re-reads it on every model call, so
    /// this applies from the session's next call — no loop restart.
    pub async fn set_time_inject(&self, id: &str, enabled: bool) -> Result<()> {
        validate_session_name(id)?;
        self.ensure_session(id).await?;
        let path = self.session_dir(id).join(crate::time_inject::MARKER);
        match crate::time_inject::marker_for(enabled) {
            None => {
                let _ = fs::remove_file(&path).await;
            }
            Some(value) => fs::write(&path, value).await?,
        }
        Ok(())
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
    /// v0.5.30: unix seconds of session creation — the `.created`
    /// marker, or the first event's `ts` for sessions created before
    /// the marker existed. Drives the client's "by creation" ordering.
    pub created: Option<u64>,
    /// M7: the session's working directory (`.cwd` marker), if set.
    /// Exposed so the client's dispatch view can group sessions by
    /// project. `None` when the marker is absent or empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// v0.5.44: the model entry this session runs with — the entry the
    /// last loop was pinned to (`.model.used`), else the pending choice
    /// (`.model`), else the global active entry. `None` only when the
    /// config cannot be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// v0.5.45: this session's reasoning effort override, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
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
        let mut rest = buf.as_slice();
        loop {
            match rest.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let line: String = String::from_utf8_lossy(&rest[..i]).into_owned();
                    rest = &rest[i + 1..];
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
        // Advance past EVERYTHING read this poll (complete lines + the
        // partial tail held in `partial`), not just the complete
        // lines: the next poll must not re-read the partial bytes, or
        // they would be prepended to the new read and duplicate the
        // line prefix (a torn poll mid-line corrupted the JSON line).
        // Truncation is detected above (offset reset to 0).
        offset += new_bytes.len() as u64;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Tail the session-local model stream channel (`.model-stream`),
/// which the model subprocess writes live SSE deltas into while a
/// model call is in flight. The loop owns the file's lifecycle: it
/// is created before each call and deleted after, so the file is
/// absent most of the time.
///
/// `catch_up` (v0.5.21): a fresh client (new browser tab, or a session
/// switch) starts with an EMPTY `live_text` (`clear_live()` on connect),
/// so it has seen none of the in-flight message's deltas. When set, the
/// tail starts at byte 0 and replays the whole current stream file first
/// (every delta the model has emitted so far), so the in-progress card
/// renders from the ACTUAL generation position instead of restarting
/// from the first delta that lands after the connect. A late
/// connection without `catch_up` (or a reconnect where the client kept
/// its stream state) starts at the current end so it is not replayed.
/// A truncate (the next model call opens with `File::create`) resets
/// the offset so the new call's deltas flow. Polls every 100 ms.
pub async fn tail_model_stream(
    path: PathBuf,
    tx: tokio::sync::mpsc::Sender<String>,
    catch_up: bool,
) {
    let mut offset: u64 = if catch_up {
        0
    } else {
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };
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
                    let mut rest = buf.as_slice();
                    loop {
                        match rest.iter().position(|&b| b == b'\n') {
                            Some(i) => {
                                let line: String = String::from_utf8_lossy(&rest[..i]).into_owned();
                                rest = &rest[i + 1..];
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
                    // Advance past EVERYTHING read this poll (complete
                    // lines + the partial tail held in `partial`), not
                    // just the complete lines: the next poll must not
                    // re-read the partial bytes, or they would be
                    // prepended to the new read and duplicate the line
                    // prefix (a torn poll mid-line corrupted the delta
                    // JSON). Truncation is still detected above via
                    // `m.len() < offset` (reset to 0).
                    offset += new_bytes.len() as u64;
                }
            }
        }

        // 60fps delivery cadence: the client renders each model_stream
        // frame within one animation frame, so polling faster than the
        // display refresh would only add wakeups without new frames.
        // (The events.jsonl tail keeps its own 50ms cadence — final
        // events don't need per-frame granularity.)
        tokio::time::sleep(std::time::Duration::from_millis(16)).await;
    }
}

/// RFC 3339 UTC timestamp with seconds precision, matching the
/// kernel's `ts` format (e.g. `2026-09-15T09:36:21Z`).
fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test process counter so parallel tests never share a root,
    /// even when the two `SystemTime::now()` calls land on the same
    /// nanosecond (which makes `remove_dir_all` in one test race the
    /// file writes of another).
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        p.push(format!(
            "rushi-web-sessions-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            seq
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mgr(root: &Path) -> SessionManager {
        SessionManager::new(Arc::new(WebConfig {
            host: "127.0.0.1".into(),
            port: 8480,
            sessions_root: root.to_path_buf(),
            loop_cmd: vec!["rushi".into(), "run".into()],
            config_path: None,
        }))
    }

    /// Write `n` event lines for session `id` (each a tiny JSON object).
    fn write_events(root: &Path, id: &str, n: u64) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut content = String::new();
        for i in 1..=n {
            content.push_str(&format!(
                r#"{{"v":1,"type":"user_message","i":{i}}}"#
            ));
            content.push('\n');
        }
        std::fs::write(dir.join("events.jsonl"), content).unwrap();
    }

    fn event_index(ev: &serde_json::Value) -> u64 {
        ev.get("i").and_then(|v| v.as_u64()).unwrap_or(0)
    }

    #[tokio::test]
    async fn events_windowed_none_gives_last_page() {
        let root = temp_root();
        write_events(&root, "s", 10);
        let sm = mgr(&root);
        let page = sm.events_windowed("s", None, 4).await.unwrap();
        assert_eq!(page.total_lines, 10);
        assert_eq!(page.oldest_line, 7);
        assert!(page.has_more);
        assert_eq!(page.events.len(), 4);
        assert_eq!(page.events.iter().map(event_index).collect::<Vec<_>>(), [7, 8, 9, 10]);
        // 10 user_message lines: first opens round 1, 9 later lines
        // open rounds 2..10.
        assert_eq!(page.total_rounds, 10);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn events_windowed_pages_backwards_to_start() {
        let root = temp_root();
        write_events(&root, "s", 10);
        let sm = mgr(&root);
        // page 1: last 4 → lines 7..10
        let p1 = sm.events_windowed("s", None, 4).await.unwrap();
        assert!(!p1.has_more == false);
        // page 2: 4 before line 7 → lines 3..6
        let p2 = sm.events_windowed("s", Some(p1.oldest_line), 4).await.unwrap();
        assert_eq!(p2.oldest_line, 3);
        assert!(p2.has_more);
        assert_eq!(p2.events.iter().map(event_index).collect::<Vec<_>>(), [3, 4, 5, 6]);
        // page 3: 4 before line 3 → lines 1..2, no older lines remain
        let p3 = sm.events_windowed("s", Some(p2.oldest_line), 4).await.unwrap();
        assert_eq!(p3.oldest_line, 1);
        assert!(!p3.has_more);
        assert_eq!(p3.events.iter().map(event_index).collect::<Vec<_>>(), [1, 2]);
        // page 4: nothing older than line 1
        let p4 = sm.events_windowed("s", Some(p3.oldest_line), 4).await.unwrap();
        assert_eq!(p4.events.len(), 0);
        assert!(!p4.has_more);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn events_windowed_single_line_log() {
        let root = temp_root();
        write_events(&root, "s", 1);
        let sm = mgr(&root);
        let page = sm.events_windowed("s", None, 200).await.unwrap();
        assert_eq!(page.total_lines, 1);
        assert_eq!(page.oldest_line, 1);
        assert!(!page.has_more);
        assert_eq!(page.events.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn events_windowed_counts_only_nonempty_lines() {
        let root = temp_root();
        let dir = root.join("s");
        std::fs::create_dir_all(&dir).unwrap();
        // 4 real events with blank lines interleaved → 4 countable lines
        std::fs::write(
            dir.join("events.jsonl"),
            r#"{"i":1}

{"i":2}
{"i":3}

{"i":4}
"#,
        )
        .unwrap();
        let sm = mgr(&root);
        let page = sm.events_windowed("s", None, 2).await.unwrap();
        assert_eq!(page.total_lines, 4);
        assert_eq!(page.oldest_line, 3);
        assert_eq!(page.events.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn events_windowed_caps_page_at_1000() {
        let root = temp_root();
        write_events(&root, "s", 10);
        let sm = mgr(&root);
        let page = sm.events_windowed("s", None, 5000).await.unwrap();
        assert_eq!(page.events.len(), 10); // cap can't exceed what exists
        assert!(!page.has_more);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn events_windowed_missing_session_errors() {
        let root = temp_root();
        let sm = mgr(&root);
        assert!(sm.events_windowed("nope", None, 100).await.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// v0.5.30: creation stamp — the `.created` marker written by
    /// `ensure_session`, the first-event-`ts` fallback for pre-marker
    /// sessions, marker precedence, and no-stamp-rewrite on re-ensure.
    #[tokio::test]
    async fn list_reports_created_from_marker_or_first_event() {
        let root = temp_root();
        let sm = mgr(&root);

        // Fresh session: the marker is written at creation time.
        sm.ensure_session("fresh").await.unwrap();
        let fresh = sm
            .list()
            .await
            .into_iter()
            .find(|s| s.name == "fresh")
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let created = fresh.created.expect("fresh session must report a stamp");
        assert!(created <= now && now - created < 5, "marker must be ~now: {created} vs {now}");

        // Pre-marker session: no marker; the first event's ts applies.
        let dir = root.join("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            "{\"v\":1,\"type\":\"user_message\",\"ts\":\"2026-01-02T03:04:05Z\"}\n\
             {\"v\":1,\"type\":\"assistant_message\",\"i\":2}\n",
        )
        .unwrap();
        let legacy = sm
            .list()
            .await
            .into_iter()
            .find(|s| s.name == "legacy")
            .unwrap();
        let expect = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .timestamp()
            .try_into()
            .unwrap();
        assert_eq!(legacy.created, Some(expect), "first-event ts must be the fallback");

        // The marker beats the first-event ts when both exist, and
        // re-ensuring an existing session must not move the stamp.
        std::fs::write(dir.join(".created"), "12345").unwrap();
        sm.ensure_session("legacy").await.unwrap();
        let legacy = sm
            .list()
            .await
            .into_iter()
            .find(|s| s.name == "legacy")
            .unwrap();
        assert_eq!(legacy.created, Some(12345), "marker wins, and is not rewritten");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Collect up to `max` messages from `rx` within a short window.
    async fn collect(rx: &mut tokio::sync::mpsc::Receiver<String>, max: usize, window_ms: u64) -> Vec<String> {
        let mut out = Vec::new();
        while out.len() < max {
            match tokio::time::timeout(std::time::Duration::from_millis(window_ms), rx.recv()).await {
                Ok(Some(l)) => out.push(l),
                _ => break,
            }
        }
        out
    }

    /// v0.5.21: with catch-up, a fresh tailer replays every complete
    /// line of the in-flight `.model-stream` file (the client's
    /// `live_text` is empty on a new connect), holds the trailing
    /// partial line for the next poll, and still resets on truncate.
    #[tokio::test]
    async fn tail_model_stream_catchup_replays_in_flight_file() {
        let dir = temp_root();
        let path = dir.join(".model-stream");
        // In-flight call: 3 complete delta lines + a partial 4th
        std::fs::write(
            &path,
            "{\"kind\":\"text\",\"delta\":\"a\"}\n\
             {\"kind\":\"text\",\"delta\":\"b\"}\n\
             {\"kind\":\"text\",\"delta\":\"c\"}\n\
             {\"kind\":\"text\",\"del",
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
        let p = path.clone();
        let task = tokio::spawn(async move { tail_model_stream(p, tx, true).await });

        let got = collect(&mut rx, 3, 300).await;
        assert_eq!(got.len(), 3, "catch-up must replay all complete lines: {got:?}");
        assert!(got[0].contains("\"a\"") && got[1].contains("\"b\"") && got[2].contains("\"c\""));
        // The partial line must NOT have been sent yet.
        assert!(rx.try_recv().is_err());

        // Writer finishes the partial line -> it flows on the next poll.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"ta\":\"d\"}\n")
            .unwrap();
        let rest = collect(&mut rx, 1, 300).await;
        assert_eq!(
            rest,
            vec!["{\"kind\":\"text\",\"delta\":\"d\"}".to_string()],
            "completed partial line must flow intact (no duplicated prefix): {rest:?}"
        );

        // Next model call truncates the file -> truncate branch resets.
        std::fs::write(&path, "{\"kind\":\"text\",\"delta\":\"n1\"}\n").unwrap();
        let fresh = collect(&mut rx, 1, 300).await;
        assert_eq!(fresh, vec!["{\"kind\":\"text\",\"delta\":\"n1\"}".to_string()]);

        drop(rx); // receiver gone; no more lines will be written, so
                  // the tailer would poll forever — abandon it
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without catch-up the tailer starts at EOF: a fresh file is NOT
    /// replayed (existing reconnects keep their own state).
    #[tokio::test]
    async fn tail_model_stream_no_catchup_starts_at_eof() {
        let dir = temp_root();
        let path = dir.join(".model-stream");
        std::fs::write(
            &path,
            "{\"kind\":\"text\",\"delta\":\"a\"}\n{\"kind\":\"text\",\"delta\":\"b\"}\n",
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
        let p = path.clone();
        let task = tokio::spawn(async move { tail_model_stream(p, tx, false).await });

        let got = collect(&mut rx, 2, 250).await;
        assert!(got.is_empty(), "no-catch-up must not replay: {got:?}");

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"kind\":\"text\",\"delta\":\"c\"}\n")
            .unwrap();
        let live = collect(&mut rx, 1, 300).await;
        assert_eq!(live.len(), 1, "new deltas must still flow: {live:?}");

        drop(rx); // receiver gone; no more lines will be written, so
                  // the tailer would poll forever — abandon it
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
