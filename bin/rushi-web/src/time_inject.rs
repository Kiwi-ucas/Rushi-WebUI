//! The `time-inject` plugin's server side — the per-session on/off toggle
//! for the harness's `time-inject` hook (`GET`/`POST
//! /api/sessions/{id}/time-inject`, wired in `main.rs`; the package is
//! `rushi-time-inject`).
//!
//! The hook (`harness-hook-time-inject`, a `model.before` hook) upserts a
//! small `"time"` fragment into every model request — when the user's last
//! message was sent, and how long the session sat idle before it. It is a
//! *pure function of the session log*, so the toggle is not a loop
//! argument: the hook re-reads it on every model call and a flip applies
//! from the session's next call, with no loop restart.
//!
//! The toggle is a one-line marker file, `sessions/<id>/.time_inject`,
//! which the hook reads through the same convention as the other plugin
//! markers:
//!
//!   * **absent** (or empty, or any value outside the off set) → **ON**,
//!     the default: a session keeps the time fragment until it opts out.
//!   * an **off** value — `off` / `no` / `0` / `false`, matched after
//!     trim + lowercase on the hook's side → **OFF**.
//!
//! This module owns that contract so the webui and the hook cannot drift:
//! same marker name, same write value, same default. `tests` below is the
//! canary — its value lists are the hook's own
//! (`rushi-time-inject/hook-time-inject/src/main.rs::time_inject_marker_gate`).

/// The marker file name. `hook-time-inject/src/main.rs` (`MARKER_NAME`)
/// and `install/TOUCHPOINTS.md` must agree with this.
pub const MARKER: &str = ".time_inject";

/// What the toggle writes for OFF. The hook trims and lowercases before
/// matching, so this exact string (not a JSON blob) is the on-disk format.
pub const OFF_VALUE: &str = "off";

/// Whether the hook injects for a session whose marker holds `raw`
/// (`None` = no marker file, or an empty one).
///
/// Mirrors the hook's `time_inject_enabled`: only the off set disables;
/// everything else — including an absent or empty marker — stays on.
pub fn enabled_from_marker(raw: Option<&str>) -> bool {
    match raw.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) => !matches!(v.as_str(), "off" | "no" | "0" | "false"),
        None => true,
    }
}

/// The marker content for a requested state: `None` = **remove** the file
/// (back to the default ON), `Some(OFF_VALUE)` = write the explicit OFF.
pub fn marker_for(enabled: bool) -> Option<&'static str> {
    if enabled { None } else { Some(OFF_VALUE) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No marker file at all → ON (the hook's default; a fresh session
    /// must get the fragment without anyone touching a file).
    #[test]
    fn absent_marker_is_on() {
        assert!(enabled_from_marker(None));
    }

    /// The hook's own off list, verbatim — including the whitespace/case
    /// sloppiness a hand-written file brings (" OFF \n").
    #[test]
    fn the_off_values_are_the_hooks_off_values() {
        for off in ["off", " OFF \n", "no", "0", "false", "False"] {
            assert!(!enabled_from_marker(Some(off)), "{off:?} should disable");
        }
    }

    /// Everything else is ON — an empty marker is the default-on path, and
    /// a stray value must never silently disable the fragment.
    #[test]
    fn every_other_value_stays_on() {
        for on in ["on", "true", "1", "yes", "", "  \n", "offx"] {
            assert!(enabled_from_marker(Some(on)), "{on:?} should stay on");
        }
    }

    /// The write path and the read path agree: disabling writes the hook's
    /// off value, enabling removes the file.
    #[test]
    fn the_write_value_round_trips_through_the_read_path() {
        assert_eq!(marker_for(true), None);
        assert_eq!(marker_for(false), Some(OFF_VALUE));
        assert!(!enabled_from_marker(marker_for(false)));
        // Removing the marker (None) is the ON path, by construction.
        match marker_for(true) {
            None => assert!(enabled_from_marker(None)),
            Some(v) => panic!("enabling must not write a value, wrote {v:?}"),
        }
    }
}
