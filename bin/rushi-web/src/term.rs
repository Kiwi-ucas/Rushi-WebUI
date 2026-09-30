//! M9: session terminal — one PTY per open terminal tab, spawned in the
//! session's working directory. The PTY belongs to the WS connection
//! that opened it; when that connection ends, the shell's process group
//! is torn down (the child calls `setsid()` at spawn, so on unix its
//! pid IS the group id), so nothing lingers after a tab close or a
//! browser refresh.
//!
//! WS frames (in-band on `/ws/sessions/{id}`):
//!   client → server:
//!     {"kind":"term_open","cols":N,"rows":M}
//!     {"kind":"term_input","data":"<base64>"}
//!     {"kind":"term_resize","cols":N,"rows":M}
//!     {"kind":"term_close"}
//!   server → client:
//!     {"kind":"term_out","data":"<base64>"}        pty master output
//!     {"kind":"term_status","running":true}        on successful open
//!     {"kind":"term_status","running":false}       on shell exit / close

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
