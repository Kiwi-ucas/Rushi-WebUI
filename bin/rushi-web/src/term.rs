//! M9: session terminal — one PTY per open terminal tab, spawned in the
//! session's working directory.
//!
//! M15: the PTY belongs to the *session*, not to the WS connection that
//! opened it. A session switch (or a page refresh) disconnects the
//! socket, but the shell keeps running in its slot; when the client's
//! socket comes back it re-attaches and the slot's `ring` replays the
//! output that arrived while it was away. Only an explicit `term_close`,
//! a session deletion, or the server exiting tears the shell's process
//! group down (the child calls `setsid()` at spawn, so on unix its pid
//! IS the group id).
//!
//! WS frames (in-band on `/ws/sessions/{id}`, multi-pty, id-tagged):
//!   client → server:
//!     {"kind":"term_open","id":N,"cols":N,"rows":M}  spawn-or-attach
//!     {"kind":"term_input","id":N,"data":"<base64>"}
//!     {"kind":"term_resize","id":N,"cols":N,"rows":M}
//!     {"kind":"term_close","id":N}                  kill the pty
//!   server → client:
//!     {"kind":"term_out","id":N,"data":"<base64>"}        pty output
//!     {"kind":"term_status","id":N,"running":true}        spawn / attach
//!     {"kind":"term_status","id":N,"running":false}       shell exit / close


use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use portable_pty::{
    Child as PtyChild, CommandBuilder, PtySize, native_pty_system,
};

pub type TermReader = Box<dyn Read + Send>;

/// One open terminal. The struct owns the pty's master side (kept alive
/// so the pty stays open while the reader/writer fd clones live), the
/// writer, and the shell child. `close()` tears down the shell's whole
/// process group; `Drop` guarantees it happens exactly once.
pub struct Term {
    child: Box<dyn PtyChild + Send + Sync>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// unix: the shell's session/process-group id (setsid ⇒ pid == pgid).
    #[allow(dead_code)]
    pgid: i32,
}

pub fn b64encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64decode(s: &str) -> Result<Vec<u8>, String> {
    B64
        .decode(s.as_bytes())
        .map_err(|e| format!("bad base64: {e}"))
}

/// Spawn the user's login shell in a fresh PTY inside `workdir`.
/// Returns the owning handle plus a blocking reader of the pty master.
pub fn open_term(
    workdir: &Path,
    cols: u32,
    rows: u32,
) -> anyhow::Result<(Term, TermReader)> {
    let pty_system = native_pty_system();
    let size = PtySize {
        rows: rows as u16,
        cols: cols as u16,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system.openpty(size).context("openpty failed")?;

    // Default program = the user's shell ($SHELL, else the password-db
    // lookup), run as a LOGIN shell (argv0 "-<name>") with the process's
    // full base environment (get_base_env), controlling tty enabled so
    // SIGWINCH drives $LINES/$COLUMNS and TIOCSWINSZ resizes land.
    let mut cmd = CommandBuilder::new_default_prog();
    cmd.cwd(workdir);
    cmd.set_controlling_tty(true);

    let child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn shell failed")?;
    let pid = child.process_id().unwrap_or(0);

    let pgid: i32 = {
        #[cfg(unix)]
        {
            pair.master
                .process_group_leader()
                .unwrap_or(pid as libc::pid_t) as i32
        }
        #[cfg(not(unix))]
        {
            pid as i32
        }
    };

    let writer = pair.master.take_writer().context("take pty writer")?;
    let reader = pair.master.try_clone_reader().context("clone pty reader")?;

    Ok((
        Term {
            child,
            writer: Mutex::new(writer),
            master: pair.master,
            pgid,
        },
        reader,
    ))
}

impl Term {
    /// Pump keystrokes into the pty. Short interactive writes; called
    /// via `spawn_blocking` from the WS loop.
    pub fn write_input(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Re-size the pty (SIGWINCH to the shell).
    pub fn resize(&self, cols: u32, rows: u32) {
        let size = PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        };
        let _ = self.master.resize(size);
    }

    /// Kill the shell AND its descendants: on unix the shell called
    /// setsid() at spawn, so its pid is a process-group id and a group
    /// signal reaches every child it started (builds, less, ...).
    ///
    /// Sequence: SIGHUP the group (a shell's clean-teardown signal,
    /// matching what a closed tty delivers), then `child.kill()`
    /// (SIGHUP → 5×50 ms grace → SIGKILL on the shell), then SIGKILL
    /// the group so nothing in it survives.
    pub fn close(&mut self) {
        #[cfg(unix)]
        {
            if self.pgid > 0 {
                unsafe {
                    let _ = libc::kill(-(self.pgid as libc::pid_t), libc::SIGHUP);
                }
            }
        }
        let _ = self.child.kill();
        #[cfg(unix)]
        {
            if self.pgid > 0 {
                unsafe {
                    let _ = libc::kill(-(self.pgid as libc::pid_t), libc::SIGKILL);
                }
            }
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        self.close();
    }
}

// ── M15: the session-scoped pty registry ────────────────────────────
//
// The PTY pool moved from "per WS connection" (M9/M11) to "per
// session": a session switch or a page refresh disconnects the socket,
// but the shells keep running here, and the reconnecting socket
// re-attaches and gets its output catch-up from each slot's `ring`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// One live (or just-dead) pty in the registry.
pub struct RegTerm {
    pub term: Term,
    /// The blocking pty-master reader. Exits at shell EOF; joined when
    /// the slot is closed / purged.
    pub reader: std::thread::JoinHandle<()>,
    /// b64 output chunks since spawn, capped at `RING_CAP` — replayed
    /// to a (re)attaching client so its terminal catches up.
    pub ring: Arc<Mutex<VecDeque<String>>>,
    /// Live fanout: one entry per attached WS connection
    /// `(attach_token, sender)`. The reader thread pushes every chunk
    /// to all of them with `blocking_send` (a bounded tokio mpsc — a
    /// slow socket back-pressures the pty read, like the original
    /// per-connection design; a closed socket's sender just fails
    /// fast and gets pruned on the next attach).
    pub live: Arc<Mutex<Vec<(u64, mpsc::Sender<(u32, Option<Vec<u8>>)>)>>>,
    /// The shell has exited; the slot stays until `close` or a
    /// `open` restart reuses the id.
    pub eof: Arc<AtomicBool>,
}

/// Ring capacity per pty (b64 chunks; ~2 MB at 4 KB chunks).
pub const RING_CAP: usize = 512;
/// How many ring entries are replayed to a (re)attaching client.
pub const RING_REPLAY: usize = 256;

/// M15: the process-wide pty registry, keyed `session -> term id`.
///
/// Spawn-or-attach: `open` reuses a live slot for its id instead of
/// spawning a second shell, so the client's "re-open a tab" path and
/// the reconnect path share one code path. Ids are client-chosen (the
/// terminal tab id, M11), which is what makes re-attach across
/// reconnects possible at all.
pub struct TermRegistry {
    map: Mutex<HashMap<String, HashMap<u32, RegTerm>>>,
}

impl TermRegistry {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    fn attach_slot(slot: &RegTerm, token: u64, tx: &Option<mpsc::Sender<(u32, Option<Vec<u8>>)>>) {
        if let Some(tx) = tx {
            let mut live = slot.live.lock().unwrap();
            // Prune closed receivers, drop any stale entry of this
            // token, then push the fresh sender (one entry per
            // connection, no duplicates).
            live.retain(|(_, s)| !s.is_closed());
            live.retain(|(t, _)| *t != token);
            live.push((token, tx.clone()));
        }
    }

    /// Spawn-or-attach pty `id` for `session` at `workdir`, attaching
    /// this connection's fanout sender (`token` + `tx`) when given.
    /// Ok = the b64 ring chunks to replay to the client (empty when a
    /// fresh pty was just spawned).
    pub fn open(
        &self,
        session: &str,
        id: u32,
        cols: u32,
        rows: u32,
        workdir: &Path,
        token: u64,
        tx: &Option<mpsc::Sender<(u32, Option<Vec<u8>>)>>,
    ) -> Result<Vec<String>, String> {
        let mut map = self.map.lock().unwrap();
        let sess = map.entry(session.to_string()).or_default();
        if let Some(slot) = sess.get(&id) {
            if !slot.eof.load(Ordering::SeqCst) {
                // Re-attach: the shell is still running (a session
                // switch or refresh dropped this id's previous socket).
                // Bring the grid back to the client's current dims and
                // hand out the ring tail.
                slot.term.resize(cols, rows);
                Self::attach_slot(slot, token, tx);
                return Ok(Self::ring_replay(slot));
            }
        }
        // Cap: at most TERM_CAP live ptys per session (a dead,
        // restartable slot does not count).
        const TERM_CAP: u32 = 4;
        let live_count = sess.values().filter(|s| !s.eof.load(Ordering::SeqCst)).count();
        if !sess.contains_key(&id) && live_count >= TERM_CAP as usize {
            return Err(format!("terminal limit ({TERM_CAP}) reached"));
        }
        // Restart path: replace a dead slot with a fresh pty under the
        // same id.
        if let Some(mut old) = sess.remove(&id) {
            old.term.close();
            let _ = old.reader.join();
        }
        let (t, reader) = open_term(workdir, cols, rows)
            .map_err(|e| format!("term_open failed: {e}"))?;
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)));
        let live: Arc<
            Mutex<Vec<(u64, mpsc::Sender<(u32, Option<Vec<u8>>)> )>>,
        > = Arc::new(Mutex::new(Vec::new()));
        let eof = Arc::new(AtomicBool::new(false));
        let r_ring = ring.clone();
        let r_live = live.clone();
        let r_eof = eof.clone();
        let handle = std::thread::spawn(move || {
            let mut r = reader;
            let mut buf = [0u8; 4096];
            use std::io::Read as _;
            loop {
                match r.read(&mut buf) {
                    Ok(0) => {
                        // EOF: the shell exited. Mark the slot dead and
                        // tell every attached connection (the slot stays
                        // in the registry so a restart can reuse the id).
                        r_eof.store(true, Ordering::SeqCst);
                        for (_tok, s) in r_live.lock().unwrap().iter() {
                            let _ = s.blocking_send((id, None));
                        }
                        break;
                    }
                    Ok(n) => {
                        // Keep the ring, then fan out to the live
                        // connections. `blocking_send` from this std
                        // thread back-pressures a slow socket (the
                        // pty read blocks); a closed socket's sender
                        // fails immediately and gets pruned next
                        // attach.
                        {
                            let mut ring = r_ring.lock().unwrap();
                            ring.push_back(b64encode(&buf[..n]));
                            while ring.len() > RING_CAP {
                                ring.pop_front();
                            }
                        }
                        for (_tok, s) in r_live.lock().unwrap().iter() {
                            let _ = s.blocking_send((id, Some(buf[..n].to_vec())));
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        let slot = RegTerm {
            term: t,
            reader: handle,
            ring,
            live,
            eof,
        };
        Self::attach_slot(&slot, token, tx);
        sess.insert(id, slot);
        Ok(Vec::new())
    }

    /// Attach this connection to EVERY pty alive in `session`
    /// (called once per WS connect, before the pump loop): returns
    /// `(id, eof, replay)` for each slot so the caller can stream the
    /// ring tail and report liveness.
    pub fn attach_all(
        &self,
        session: &str,
        token: u64,
        tx: &mpsc::Sender<(u32, Option<Vec<u8>>)>,
    ) -> Vec<(u32, bool, Vec<String>)> {
        let tx = Some(tx.clone());
        let mut out = Vec::new();
        if let Some(sess) = self.map.lock().unwrap().get_mut(session) {
            for (id, slot) in sess.iter() {
                Self::attach_slot(slot, token, &tx);
                out.push((*id, slot.eof.load(Ordering::SeqCst), Self::ring_replay(slot)));
            }
        }
        out
    }

    /// Detach one connection (WS disconnect): drop its fanout entries.
    /// The shells keep running; their output accumulates in the rings.
    pub fn detach(&self, session: &str, token: u64) {
        if let Some(sess) = self.map.lock().unwrap().get_mut(session) {
            for slot in sess.values() {
                let mut live = slot.live.lock().unwrap();
                live.retain(|(t, _)| *t != token);
            }
        }
    }

    /// Kill just pty `id` (explicit `term_close`).
    pub fn close(&self, session: &str, id: u32) -> bool {
        let mut map = self.map.lock().unwrap();
        let Some(sess) = map.get_mut(session) else {
            return false;
        };
        let Some(mut slot) = sess.remove(&id) else {
            return false;
        };
        slot.term.close();
        let _ = slot.reader.join();
        true
    }

    /// Does pty `id` exist in `session` (live or dead-restartable)?
    pub fn has(&self, session: &str, id: u32) -> bool {
        self.map
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|sess| sess.contains_key(&id))
    }

    /// Pump keystrokes into pty `id` (a no-op when the pty is gone).
    pub fn input(&self, session: &str, id: u32, bytes: &[u8]) {
        if let Some(sess) = self.map.lock().unwrap().get(session) {
            if let Some(slot) = sess.get(&id) {
                slot.term.write_input(bytes);
            }
        }
    }

    /// Re-size pty `id` (SIGWINCH; a no-op when the pty is gone).
    pub fn resize(&self, session: &str, id: u32, cols: u32, rows: u32) {
        if let Some(sess) = self.map.lock().unwrap().get(session) {
            if let Some(slot) = sess.get(&id) {
                slot.term.resize(cols, rows);
            }
        }
    }

    /// Kill every pty of `session` (session deletion / shutdown).
    pub fn purge(&self, session: &str) {
        let mut map = self.map.lock().unwrap();
        if let Some(sess) = map.remove(session) {
            for mut slot in sess.into_values() {
                slot.term.close();
                let _ = slot.reader.join();
            }
        }
    }

    fn ring_replay(slot: &RegTerm) -> Vec<String> {
        let ring = slot.ring.lock().unwrap();
        ring.iter()
            .rev()
            .take(RING_REPLAY)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}
