//! Read-only view of a session's essence store (`essence.json`).
//!
//! Mirrors the `rushi-essence` on-disk layout: `essence.json` holds a store
//! envelope whose `entries` array carries the session's invariants + beliefs
//! (the harness's `essence` tool owns all writes; the web server only reads
//! it, so the sidebar essence plugin is a read-only display).

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One essence entry. Field set mirrors `rushi-essence/essence-state::Entry`;
/// `kind` is a string ("invariant" | "belief") so the web side needs no copy
/// of the kernel's `Kind` enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EssenceEntry {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub survivals: u32,
    #[serde(default)]
    pub last_seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retracted_reason: Option<String>,
}

/// The on-disk store envelope.
#[derive(Debug, Clone, Default, Deserialize)]
struct EssenceStore {
    #[serde(default)]
    entries: Vec<EssenceEntry>,
}

/// API-facing payload.
#[derive(Debug, Clone, Serialize)]
pub struct EssenceView {
    pub entries: Vec<EssenceEntry>,
}

/// Read a session's essence entries (empty if the file is absent or
/// unparsable — e.g. mid-write by the harness). Sorted for display: active
/// invariants, active beliefs, then demoted (invariants before beliefs).
pub fn read(session_dir: &Path) -> EssenceView {
    let data = match fs::read_to_string(session_dir.join("essence.json")) {
        Ok(d) if !d.trim().is_empty() => d,
        _ => return EssenceView { entries: vec![] },
    };
    let store: EssenceStore = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(_) => return EssenceView { entries: vec![] },
    };
    let mut entries = store.entries;
    entries.sort_by(|a, b| {
        let rank = |e: &EssenceEntry| match (e.kind.as_str(), e.active) {
            ("invariant", true) => 0,
            ("belief", true) => 1,
            ("invariant", false) => 2,
            _ => 3,
        };
        rank(a).cmp(&rank(b)).then_with(|| a.id.cmp(&b.id))
    });
    EssenceView { entries }
}
