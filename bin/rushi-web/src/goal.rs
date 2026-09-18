//! Goal state management — mirrors the `goal-state` crate's on-disk
//! layout (goal.json pointer + goal-<id>.json per-goal files) so the
//! web UI and the kernel hooks stay in sync through the same files.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

// ── on-disk types (match the goal-state crate) ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub id: String,
    pub goal: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub used_tokens: u64,
    #[serde(default)]
    pub iteration: u64,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoalPointer {
    pub current_goal: String,
}

impl GoalState {
    fn path(session_dir: &Path) -> std::path::PathBuf {
        session_dir.join("goal.json")
    }

    fn goal_file(session_dir: &Path, id: &str) -> Option<std::path::PathBuf> {
        if id.is_empty() || id == "." || id == ".." || id.starts_with('-')
            || id.contains('/') || id.contains('\\') || id.contains('\0')
        {
            return None;
        }
        Some(session_dir.join(format!("goal-{id}.json")))
    }

    /// Load the current goal state (follows the pointer).
    pub fn load(session_dir: &Path) -> Option<GoalState> {
        let p = Self::path(session_dir);
        let data = fs::read_to_string(&p).ok()?;
        if data.trim().is_empty() {
            return None;
        }
        // New layout: pointer → per-goal file
        if let Ok(ptr) = serde_json::from_str::<GoalPointer>(&data) {
            let gp = Self::goal_file(session_dir, &ptr.current_goal)?;
            let gd = fs::read_to_string(&gp).ok()?;
            return serde_json::from_str(&gd).ok();
        }
        // Legacy: goal.json holds full state
        serde_json::from_str(&data).ok()
    }

    /// List every goal in the session (per-goal files + legacy).
    pub fn list(session_dir: &Path) -> Vec<GoalState> {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(session_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name.strip_prefix("goal-").and_then(|s| s.strip_suffix(".json")) {
                    if let Some(gp) = Self::goal_file(session_dir, id) {
                        if let Ok(d) = fs::read_to_string(&gp) {
                            if let Ok(g) = serde_json::from_str::<GoalState>(&d) {
                                out.push(g);
                            }
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn save(&self, session_dir: &Path) -> Result<(), String> {
        let gf = Self::goal_file(session_dir, &self.id)
            .ok_or_else(|| "unsafe goal id".to_string())?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&gf, json).map_err(|e| e.to_string())?;

        let ptr = GoalPointer {
            current_goal: self.id.clone(),
        };
        let pjson = serde_json::to_string_pretty(&ptr).map_err(|e| e.to_string())?;
        fs::write(Self::path(session_dir), pjson).map_err(|e| e.to_string())
    }

    fn clear(session_dir: &Path) -> bool {
        fs::remove_file(Self::path(session_dir)).is_ok()
    }
}

// ── API-facing types ───────────────────────────────────────────────

#[derive(Serialize)]
pub struct GoalView {
    pub current: Option<GoalState>,
    pub all: Vec<GoalState>,
}

#[derive(Deserialize)]
pub struct GoalAction {
    pub action: String,
    #[serde(default)]
    pub goal: Option<String>,
}

/// Generate a goal id: g- + 8 hex from time nanos.
fn gen_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let mixed = (nanos.wrapping_mul(0x9E37_79B9)) & 0xFFFF_FFFF;
    format!("g-{mixed:08x}")
}

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("t+{secs}s")
}

/// Handle a goal action from the web UI.
pub fn apply_action(session_dir: &Path, action: &GoalAction) -> Result<GoalState, String> {
    match action.action.as_str() {
        "create" => {
            let text = action
                .goal
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("goal text required for create")?;
            let g = GoalState {
                id: gen_id(),
                goal: text.to_string(),
                active: true,
                used_tokens: 0,
                iteration: 0,
                completed: false,
                blocked: false,
                block_reason: None,
                opened_at: Some(now_stamp()),
                closed_at: None,
            };
            g.save(session_dir)?;
            Ok(g)
        }
        "pause" => {
            let mut g = GoalState::load(session_dir).ok_or("no active goal")?;
            g.active = false;
            g.save(session_dir)?;
            Ok(g)
        }
        "resume" => {
            let mut g = GoalState::load(session_dir).ok_or("no active goal")?;
            g.active = true;
            g.blocked = false;
            g.completed = false;
            g.block_reason = None;
            g.closed_at = None;
            g.used_tokens = 0;
            g.iteration = 0;
            g.opened_at = Some(now_stamp());
            g.save(session_dir)?;
            Ok(g)
        }
        "clear" => {
            GoalState::clear(session_dir);
            Err("cleared".to_string())
        }
        "edit" => {
            let mut g = GoalState::load(session_dir).ok_or("no active goal")?;
            let text = action
                .goal
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("goal text required for edit")?;
            g.goal = text.to_string();
            g.save(session_dir)?;
            Ok(g)
        }
        _ => Err(format!("unknown action: {}", action.action)),
    }
}

/// Read goal state for a session (current + full list).
pub fn read(session_dir: &Path) -> GoalView {
    let current = GoalState::load(session_dir);
    let all = GoalState::list(session_dir);
    GoalView { current, all }
}
