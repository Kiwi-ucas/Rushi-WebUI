use std::cell::RefCell;

use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── model settings (v0.5.42) ────────────────────────────────────────
//
// Wire mirror of `bin/rushi-web/src/modelcfg.rs`. Every field is
// `#[serde(default)]` so a server that grows or drops a key cannot fail
// the whole payload. Change both sides together.

/// One `[model."<name>"]` entry; `None`/empty means "key absent — the
/// kernel's default applies".
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ModelEntry {
    pub name: String,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub vision: Option<bool>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub estimate_chars_per_token: Option<u64>,
    /// Read-only: keys the panel does not edit (e.g. `api`).
    #[serde(default)]
    pub extra_keys: Vec<String>,
    /// v0.5.47: the name this entry was loaded under, so the server can
    /// tell a rename from a delete + add and keep the entry's place in
    /// `[model]`. `None` for an entry the form just created; never sent
    /// back by the server.
    #[serde(default)]
    pub orig_name: Option<String>,
}

/// The five `[model]` keys the kernel falls back to.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Globals {
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub model_timeout_s: Option<u64>,
    #[serde(default)]
    pub estimate_chars_per_token: Option<u64>,
    #[serde(default)]
    pub vision: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelSettingsView {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub entries: Vec<ModelEntry>,
    #[serde(default)]
    pub globals: Globals,
    #[serde(default)]
    pub config_path: String,
    #[serde(default)]
    pub mirror_path: Option<String>,
    #[serde(default)]
    pub key_env_present: std::collections::HashMap<String, bool>,
    /// Which of those come from `config.secrets.toml` (a key pasted in
    /// the panel) rather than the server's environment.
    #[serde(default)]
    pub key_env_stored: std::collections::HashMap<String, bool>,
    #[serde(default)]
    pub effective: Option<Value>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// One session row in the sidebar (port of JS `loadSessions`).
#[derive(Clone, Debug, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    #[serde(default)]
    pub last_modified: Option<f64>,
    /// v0.5.30: unix seconds of session creation (the server's
    /// `.created` marker or first-event ts; None for old servers).
    #[serde(default)]
    pub created: Option<u64>,
    /// M7: the session's working directory (its `.cwd` marker), if the
    /// server reports one. The dispatch view groups sessions by this
    /// (project); None groups under "no project".
    #[serde(default)]
    pub cwd: Option<String>,
    /// v0.5.44: the model entry this session runs with — its own choice,
    /// else the entry the last loop used, else the config's active one.
    #[serde(default)]
    pub model: Option<String>,
}

/// M7: project groups for the dispatch view (layout "full") — the
/// ordered session list split by working directory. Sessions without
/// a `.cwd` marker (or old servers that never report one) share the
/// "(no project)" bucket. Group order = first appearance of each
/// group in the given session ordering, so the ordering mode still
/// governs which project sits on top.
pub fn dispatch_groups(
    sessions: &[SessionInfo],
    mode: &str,
    custom_order: &[String],
    rank: &std::collections::HashMap<String, f64>,
) -> Vec<(String, Vec<SessionInfo>)> {
    let ordered = ordered_sessions(sessions, mode, custom_order, rank);
    let mut groups: Vec<(String, Vec<SessionInfo>)> = Vec::new();
    let mut idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for s in ordered {
        let key = s
            .cwd
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "(no project)".to_string());
        let i = *idx.entry(key.clone()).or_insert_with(|| {
            groups.push((key.clone(), Vec::new()));
            groups.len() - 1
        });
        groups[i].1.push(s.clone());
    }
    groups
}

/// The short label for a dispatch group header: the last path
/// component of the project directory ("no project" as-is).
pub fn dispatch_group_label(key: &str) -> String {
    if key == "(no project)" {
        return key.to_string();
    }
    key.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(key)
        .to_string()
}

// ── M8: right tool panel (Files tree + preview) ─────────────────────

/// M8: one entry in a Files-tree directory listing (`GET /api/files`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    #[serde(default)]
    pub size: Option<u64>,
}

/// M8: server response for `GET /api/files` — one directory's listing.
#[derive(Clone, Debug, Deserialize)]
pub struct DirList {
    /// The session's working directory (absolute). Part of the wire
    /// contract; the tree currently keys off `entries` only.
    #[allow(dead_code)]
    pub cwd: String,
    /// The request's relative path ("" = root). Wire contract.
    #[allow(dead_code)]
    #[serde(default)]
    pub path: String,
    pub entries: Vec<DirEntry>,
}

/// M8: server response for `GET /api/file` (text/code preview payload).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FilePreview {
    pub path: String,
    pub content: String,
    pub size: u64,
    #[serde(default)]
    pub truncated: bool,
}

// ── M11: browser-style multi-tab right panel ────────────────────────
/// The kind of one right-panel tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RpTabKind {
    /// The Files tree. M13: no longer a permanent home tab — the panel
    /// starts empty (a "start" view with Files/Terminal launcher
    /// buttons); a Files tab is opened on demand (id 0, empty path) and
    /// is closable like the others.
    Files,
    /// A text/code/image preview of one file (closable).
    File,
    /// A live terminal (closable → `term_close`).
    Term,
}

/// One open right-panel tab. The `Files` tab has `id == 0`, an empty
/// `path` and the label "Files" (re-opened after closing always
/// re-uses id 0, so the per-tab error/preview slots keyed at 0 stay
/// valid). File tabs carry the workdir-relative `path`; terminal tabs
/// carry an empty `path` and the label "Term N". Terminal tab ids
/// double as the server's pty ids, so a tab closing maps 1:1 onto
/// `term_close{id}`.
#[derive(Clone, Debug, PartialEq)]
pub struct RpTab {
    pub id: u32,
    pub kind: RpTabKind,
    /// Workdir-relative path (File tabs only; empty otherwise).
    pub path: String,
    /// Display label ("Files" / file name / "Term N").
    pub label: String,
}

/// Per-terminal-tab liveness, keyed by the terminal tab id. Drives the
/// status-bar lamp and the "shell exited — restart" overlay.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TermState {
    /// Last `term_status{id, running}` for this terminal.
    pub running: bool,
    /// Whether the server ever reported this terminal running — so the
    /// exit overlay only appears after a shell has actually started and
    /// then died (never on first paint / spawn failure).
    pub started: bool,
    /// A spawn/limit error from `term_status{id, running:false, error}`.
    pub error: Option<String>,
}

/// v0.5.30: the sidebar's session order for the three modes. The
/// result is deterministic (ties break by name), so re-renders never
/// reshuffle the list:
/// - "created": newest creation on top (`created` desc; None = oldest).
/// - "output": by `rank` stamp (loop-completion stamps seeded from
///   `last_modified`), desc.
/// - "custom": the user's `custom_order`; sessions not in the list
///   append at the end in name order (a new session never reshuffles
///   the user's arrangement).
pub fn ordered_sessions(
    sessions: &[SessionInfo],
    mode: &str,
    custom_order: &[String],
    rank: &std::collections::HashMap<String, f64>,
) -> Vec<SessionInfo> {
    let mut out = sessions.to_vec();
    out.sort_by(|a, b| {
        let tie = a.name.cmp(&b.name);
        match mode {
            "created" => b
                .created
                .unwrap_or(0)
                .cmp(&a.created.unwrap_or(0))
                .then_with(|| tie),
            "custom" => {
                let pos = |n: &str| {
                    custom_order
                        .iter()
                        .position(|x| x == n)
                        .map(|p| p as u64)
                        .unwrap_or(u64::MAX)
                };
                pos(&a.name).cmp(&pos(&b.name)).then_with(|| tie)
            }
            // "output" (and unknown values): by the rank stamp.
            _ => {
                let key = |s: &SessionInfo| {
                    rank.get(&s.name)
                        .copied()
                        .unwrap_or(s.last_modified.unwrap_or(0.0))
                };
                key(b)
                    .partial_cmp(&key(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| tie)
            }
        }
    });
    out
}

/// Goal panel payload (port of JS `loadGoal` / goal view).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GoalView {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub block_reason: Option<String>,
    #[serde(default)]
    pub iteration: u64,
    #[serde(default)]
    pub used_tokens: u64,
}

impl GoalView {
    /// Badge status (port of the legacy goal-badge logic):
    /// blocked > completed > paused > active.
    pub fn status(&self) -> &'static str {
        if self.blocked {
            "blocked"
        } else if self.completed {
            "completed"
        } else if !self.active {
            "paused"
        } else {
            "active"
        }
    }
}

/// v0.5.56: the session's time-inject toggle state (sidebar time plugin).
/// The server mirrors the harness hook's marker
/// (`sessions/<id>/.time_inject`, written by the plugin's toggle): a
/// missing marker is ON (the hook default), an explicit off value is OFF.
#[derive(Clone, Debug, Deserialize)]
pub struct TimeInjectView {
    #[serde(default = "default_time_inject_enabled")]
    pub enabled: bool,
}

fn default_time_inject_enabled() -> bool {
    true
}

/// v0.5.54: one session-essence entry (sidebar essence plugin, read-only).
/// Mirrors the harness `essence` store (`rushi-essence/essence-state::Entry`,
/// `sessions/<id>/essence.json`). `kind` is "invariant" or "belief".
#[derive(Clone, Debug, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retracted_reason: Option<String>,
}

/// Rewind plugin: one node of the conversation history tree = one user
/// message = one loop round. Projected server-side from `events.jsonl`
/// (`GET /api/sessions/{id}/rewind`); rendered by the full-window History
/// view and summarized in the sidebar plugin panel.
#[derive(Clone, Debug, Deserialize)]
pub struct RewindNode {
    /// 1-based log line of this round's user message (the rewind target).
    pub seq: u64,
    /// 1-based round index.
    #[serde(default)]
    pub round: u64,
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub summary: String,
    /// Non-`ext_status` events folded into this round (the agent's work).
    #[serde(default)]
    pub events: u64,
    /// "active" (on the active path) | "abandoned" (a masked fork).
    #[serde(default)]
    pub state: String,
    /// The "you are here" round.
    #[serde(default)]
    pub current: bool,
    #[serde(default)]
    pub retracted: bool,
    #[serde(default)]
    pub children: Vec<RewindNode>,
}

/// Rewind plugin: one `rewind` marker (a fork point) at its log line.
#[derive(Clone, Debug, Deserialize)]
pub struct RewindMarker {
    pub seq: u64,
    pub target_seq: u64,
    #[serde(default)]
    pub mode: String,
}

/// Rewind plugin: a compaction boundary — a rewind targeting below
/// `first_kept_seq` degrades to the boundary (kernel rule I4).
#[derive(Clone, Debug, Deserialize)]
pub struct RewindBoundary {
    pub seq: u64,
    pub first_kept_seq: u64,
}

/// Rewind plugin: the projected history tree of one session.
#[derive(Clone, Debug, Deserialize)]
pub struct RewindTree {
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub total_events: u64,
    #[serde(default)]
    pub total_rounds: u64,
    #[serde(default)]
    pub roots: Vec<RewindNode>,
    #[serde(default)]
    pub rewinds: Vec<RewindMarker>,
    #[serde(default)]
    pub boundaries: Vec<RewindBoundary>,
    /// The "you are here" round's log line.
    #[serde(default)]
    pub current_seq: Option<u64>,
    /// Where the next context assembly ends (equal to `current_seq`).
    #[serde(default)]
    pub pending_from: Option<u64>,
    /// The log's tail is a rewind marker (settled at the target).
    #[serde(default)]
    pub settled: bool,
}

impl RewindTree {
    fn count(nodes: &[RewindNode]) -> usize {
        nodes.iter().map(|n| 1 + Self::count(&n.children)).sum()
    }
    fn active_count(nodes: &[RewindNode]) -> usize {
        nodes
            .iter()
            .filter(|n| n.state == "active")
            .map(|n| 1 + Self::active_count(&n.children))
            .sum()
    }
    fn find_current(nodes: &[RewindNode]) -> Option<&RewindNode> {
        for n in nodes {
            if n.current {
                return Some(n);
            }
            if let Some(x) = Self::find_current(&n.children) {
                return Some(x);
            }
        }
        None
    }

    /// Every round in the tree (active + abandoned).
    pub fn rounds(&self) -> usize {
        Self::count(&self.roots)
    }
    /// Rounds on the active path.
    pub fn active_rounds(&self) -> usize {
        Self::active_count(&self.roots)
    }
    /// Abandoned (masked-fork) rounds.
    pub fn abandoned_rounds(&self) -> usize {
        self.rounds().saturating_sub(self.active_rounds())
    }
    /// The "you are here" node, if any.
    pub fn current_node(&self) -> Option<&RewindNode> {
        Self::find_current(&self.roots)
    }
    fn collect_active(nodes: &[RewindNode], out: &mut Vec<RewindNode>) {
        for n in nodes {
            if n.state == "active" {
                out.push(n.clone());
                Self::collect_active(&n.children, out);
            }
        }
    }
    /// The active path in order (root → current), one node per round.
    /// An abandoned node cannot have active descendants (the active path is
    /// a single root-to-cursor chain), so abandoned subtrees are skipped.
    pub fn active_path(&self) -> Vec<RewindNode> {
        let mut v = Vec::new();
        Self::collect_active(&self.roots, &mut v);
        v
    }
}

/// Rewind plugin: the pending rewind target (the confirm dialog's state).
#[derive(Clone, Debug, PartialEq)]
pub struct RewindTarget {
    /// The target round's 1-based log line.
    pub seq: u64,
    /// Display label ("round 3 · first 40 chars…").
    pub label: String,
}

impl EssenceEntry {
    /// Display grouping: active invariants, active beliefs, then the
    /// demoted (inactive) ones.
    pub fn rank(&self) -> u8 {
        match (self.kind.as_str(), self.active) {
            ("invariant", true) => 0,
            ("belief", true) => 1,
            ("invariant", false) => 2,
            _ => 3,
        }
    }
}

/// A closed/in-flight round = a run of events starting at a user_message.
pub type Round = std::ops::Range<usize>;

/// Split the event stream into rounds, mirroring the legacy bookkeeping:
/// every user_message closes the previous round and opens a new one.
pub fn compute_rounds(events: &[Value]) -> Vec<Round> {
    let n = events.len();
    if n == 0 {
        return Vec::new();
    }
    let mut starts = vec![0usize];
    for (i, ev) in events.iter().enumerate() {
        if i == 0 {
            continue;
        }
        if ev.get("type").and_then(|v| v.as_str()) == Some("user_message") {
            starts.push(i);
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| s..starts.get(k + 1).copied().unwrap_or(n))
        .collect()
}

/// v0.5.38: content of the most recent `user_message` in a (possibly
/// windowed) event slice — the instruction the user most recently sent.
/// Empty when the slice holds no user message. Pure function over a
/// slice so it can run on the truncated WS window OR a full fetch.
/// The loop-cmd chip (`AppState::loop_cmd`) is kept fresh by mirroring
/// this over the loaded window (lib.rs effect) and seeded by a full
/// fetch when a session's command is buried under >HIST_PAGE events.
pub fn last_user_command_slice(events: &[Value]) -> String {
    events
        .iter()
        .rev()
        .find(|ev| ev.get("type").and_then(|v| v.as_str()) == Some("user_message"))
        .and_then(|ev| ev.get("content").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

/// Shared reactive app state (port of the JS module-level globals).
#[derive(Clone, Copy)]
pub struct AppState {
    pub sessions: RwSignal<Vec<SessionInfo>>,
    pub active_session: RwSignal<Option<String>>,
    /// Normalized event stream (JSON values, matching the JS `events` array).
    pub events: RwSignal<Vec<Value>>,
    /// v0.5.33: identity generation of the `events` list. Bumped on
    /// every PREFIX-CHANGING write (initial `history` frame,
    /// `load_earlier` prepend) so the transcript's `For` can key its
    /// cards by (gen, index): a plain index key would keep the
    /// positional views alive across a prepend (Leptos re-diffs by
    /// key — same index = same view = the pre-prepended card), which
    /// silently swallowed every "load earlier" page. Streaming
    /// appends do NOT bump it, so the hot path keeps stable keys.
    pub ev_gen: RwSignal<u64>,
    /// v0.5.52: incremental input for the status strip — the LATEST
    /// value per `ext_status` id, maintained in O(1) at the append
    /// sites instead of re-scanning the whole event window (and
    /// rebuilding the chip views) on every appended event. See plan
    /// §9.13/§9.14 (P2).
    pub last_ext: RwSignal<std::collections::HashMap<String, String>>,
    /// v0.5.52: bumped only when the last `user_message` may have
    /// changed (append / history replace / prepend / optimistic send /
    /// clear). The loop-cmd Effect watches THIS instead of the whole
    /// event list, so an ordinary event no longer re-derives the chip.
    pub cmd_gen: RwSignal<u64>,
    /// v0.5.17: 1-based line number of the OLDEST event currently in
    /// `events` (truncated history: the server ships only the last
    /// page on connect; 1 = the whole log is loaded). 0 = unknown.
    pub hist_oldest_line: RwSignal<u64>,
    /// v0.5.17: whether older events exist above the loaded window.
    pub hist_has_more: RwSignal<bool>,
    /// v0.5.17: true while a "load earlier" page request is in flight
    /// (guards against double-firing the button).
    pub loading_earlier: RwSignal<bool>,
    /// v0.5.21: total rounds across the FULL log, as counted by the
    /// server (1 + user_message lines). 0 = unknown (old server). Drives
    /// global round numbering on the chips while the window is truncated.
    pub hist_total_rounds: RwSignal<u64>,
    /// v0.5.21: how many events "load earlier" has prepended on this
    /// connection (0 = only the initial page). Drives the "N loaded"
    /// hint on the load-earlier pill.
    pub earlier_loaded: RwSignal<u64>,
    /// v0.5.21: the last "load earlier" attempt failed (server error
    /// frame, or the page never arrived before the watchdog fired).
    /// The pill shows a retry hint until the next attempt succeeds.
    pub earlier_failed: RwSignal<bool>,
    pub ws_status: RwSignal<String>,
    pub loop_running: RwSignal<bool>,
    pub ctx_used: RwSignal<u64>,
    pub goal: RwSignal<Option<GoalView>>,
    /// v0.5.56: the time plugin (sidebar): the active session's
    /// time-inject toggle state. None until the first fetch. The marker
    /// is read live by the harness hook on every model call, so the
    /// server's answer is the current state.
    pub time_inject: RwSignal<Option<TimeInjectView>>,
    /// v0.5.53: the sidebar plugin area. `active_plugin` is the id of the
    /// plugin rendered in `#plugin-view` (registry: `plugins.rs`);
    /// `plugin_menu_open` toggles the picker dropdown in `#plugin-bar`.
    pub active_plugin: RwSignal<String>,
    pub plugin_menu_open: RwSignal<bool>,
    /// None = live view; Some(i) = show only round i (0-based).
    pub view_round: RwSignal<Option<usize>>,
    /// M6: layout mode "main" | "split" | "full", persisted per browser.
    /// "main" hides the sidebar, "full" is the full-screen dispatch view.
    pub layout_mode: RwSignal<String>,
    /// Open session-row context menu: which session (None = closed).
    /// Shared so the document-level click/Escape close handlers (ported
    /// from the legacy document listeners) can reach it.
    pub menu_session: RwSignal<Option<String>>,
    /// Menu anchor position (click x/y) for the floating session menu.
    pub menu_pos: RwSignal<(f64, f64)>,
    /// Context usage recorded at each round close (legacy `rounds[].ctxK`);
    /// index i = round i. Shown as "~K" on the ctx chips.
    pub rounds_ctxk: RwSignal<Vec<u64>>,
    /// Number of cards currently folded into the pile (drives the
    /// pile-stack header). Updated by the engine every step.
    pub pile_count: RwSignal<usize>,
    /// Brief of the top (newest) folded card, shown on the pile-stack.
    pub pile_top_brief: RwSignal<String>,
    /// Whether the pile is expanded (click / scroll-to-top deals all).
    pub pile_open: RwSignal<bool>,
    /// New-session dialog: Some(default_cwd) shows the modal.
    pub new_session_open: RwSignal<Option<String>>,
    /// Delete-confirmation sub-window: Some(name) shows the dialog.
    pub confirm_delete: RwSignal<Option<String>>,
    /// Accumulated live text deltas of the in-flight model call
    /// (`model_stream` frames). Rendered by the streaming card and
    /// dropped when the final `assistant_message` event lands.
    pub live_text: RwSignal<String>,
    /// Accumulated live reasoning deltas of the in-flight call (the
    /// streaming "thinking" block). Dropped with the final event.
    pub live_reasoning: RwSignal<String>,
    /// True while `model_stream` deltas are flowing (the streaming
    /// card is visible and the pile follows it). Cleared when the
    /// final `assistant_message` / `error` event lands, on history
    /// reload, or on session switch.
    pub streaming: RwSignal<bool>,
    /// v0.5.15: one-frame handoff marker — set when the
    /// `assistant_message` that finalized a live stream lands, so the
    /// transcript mounts that card with the `.ev-settling` animation
    /// (it starts in the in-flight card's dark/lifted state and glides
    /// to the settled light face — no color step). Consumed by the
    /// card that mounts; cleared on every other event frame, history
    /// load and session switch (ws.rs / clear_live).
    pub settling_card: RwSignal<bool>,
    /// `tool_call` ids that have no `tool_result` yet: their cards
    /// show a running state until the result event arrives.
    pub tool_pending: RwSignal<Vec<String>>,
    /// v0.5.8 (flat mode only): the round currently under the
    /// viewport according to the scroll position — drives the chip
    /// row. Written by the pile engine's flat-mode scroll→round
    /// detector; pile mode leaves it None (chips are driven by
    /// `view_round` there), so the chip "on" state ORs the two.
    pub round_active: RwSignal<Option<usize>>,
    /// v0.5.13: sessions that have a live loop process RIGHT NOW
    /// (server-driven: the "loops" snapshot on connect + each
    /// "loop_status" frame). The sidebar shows a breathing lamp on
    /// ANY of these — not just the session being viewed.
    pub looping_sessions: RwSignal<std::collections::HashSet<String>>,
    /// v0.5.13: sessions whose loop ended but which have not been
    /// re-viewed since: the sidebar card shows a static green lamp
    /// until the user opens the session; leaving it again clears it.
    pub loop_done_unviewed: RwSignal<std::collections::HashSet<String>>,
    /// v0.5.55 P1: sessions the user has *viewed* this app session. A
    /// session's finished-loop green lamp is only re-seeded by the
    /// `GET /api/loops` poll while the session is NOT in this set — so
    /// once the user opens a session, its "finished, unviewed" lamp does
    /// not come back on the next 10 s poll. Cleared on page reload
    /// (in-memory only; the server-side `loop.last` deletion is what
    /// keeps a reload from re-lighting it).
    pub loop_viewed: RwSignal<std::collections::HashSet<String>>,
    /// v0.5.22: theme mode — "auto" (follow the OS color scheme),
    /// "light" or "dark". Persisted per browser (localStorage
    /// "rushi-theme"); the EFFECTIVE theme ("light"/"dark") is written
    /// to `<html data-theme>` by ui::theme_apply and drives the CSS
    /// token overrides (`html[data-theme="dark"]` in style.css).
    pub theme_mode: RwSignal<String>,
    /// v0.5.30: sidebar ordering mode — "created" (newest creation on
    /// top), "output" (the order only moves when a loop COMPLETES,
    /// never per streamed card), or "custom" (the user's own drag
    /// order; the other two modes are ignored while it is active).
    /// Persisted per browser (localStorage "rushi-sort-mode").
    pub sort_mode: RwSignal<String>,
    /// v0.5.30: the user's own session order (custom mode) — session
    /// names; sessions missing from the list append at the end in
    /// name order. Persisted as "rushi-custom-order" (JSON array).
    pub custom_order: RwSignal<Vec<String>>,
    /// v0.5.30: "output" mode rank — session → stamp (unix seconds).
    /// Seeded from the server's `last_modified` when a session is
    /// first observed; bumped ONLY when the session's loop completes
    /// (the `loop_status` running=false frame in ws.rs), so three
    /// concurrently streaming sessions do not reshuffle the sidebar.
    pub output_rank: RwSignal<std::collections::HashMap<String, f64>>,
    /// v0.5.30: transient drag state (custom mode only) — the session
    /// being dragged and the item under the cursor.
    pub dragging_session: RwSignal<Option<String>>,
    pub drop_target: RwSignal<Option<String>>,
    /// v0.5.38: the LAST user instruction of the ACTIVE session, taken
    /// from a FULL transcript fetch (not the truncated WS window) so it
    /// is correct even when the command is buried under more than
    /// HIST_PAGE model/tool events (e.g. a long-running loop). Fed to
    /// the loop-cmd chip only when the loaded window has no user message.
    /// Refreshed in lib.rs when the active session changes.
    pub loop_cmd: RwSignal<String>,
    /// v0.5.38: which session `loop_cmd` belongs to (guard so the
    /// refresh only re-fetches on a session change, not every poll).
    pub loop_cmd_sess: RwSignal<Option<String>>,
    /// M8: right tool panel open/closed (persisted as "rushi-rp-open").
    /// The panel is a session-scoped inspector: Files tree + preview.
    pub rp_open: RwSignal<bool>,
    /// M11: the open right-panel tabs (browser-style). The Files home
    /// tab (id 0) is always present; file/terminal tabs are created on
    /// demand (open a file / new terminal) and closed via the tab
    /// button. A terminal tab's `id` doubles as the server pty id.
    pub rp_tabs: RwSignal<Vec<RpTab>>,
    /// M11: the active right-panel tab id (default 0 = Files home).
    pub rp_active: RwSignal<u32>,
    /// M11: monotonically increasing id handed out to new tabs.
    pub rp_next_id: RwSignal<u32>,
    /// M11: next "Term N" label number for new terminal tabs.
    pub rp_term_seq: RwSignal<u32>,
    /// M8: expanded directories in the Files tree, as workdir-relative
    /// paths ("": the workdir root; "src" / "src/lib.rs"'s parent …).
    pub rp_expanded: RwSignal<std::collections::HashSet<String>>,
    /// M8: lazily fetched directory listings: workdir-relative dir path
    /// ("" = root) → its entries. Populated on demand by the tree.
    pub rp_children: RwSignal<std::collections::HashMap<String, Vec<DirEntry>>>,
    /// M11: per-tab file preview payload, keyed by the FILE tab's id
    /// (`None` = still loading / not fetched yet; absent = not open).
    pub rp_preview_map: RwSignal<std::collections::HashMap<u32, Option<FilePreview>>>,
    /// M11: per-tab error line (preview / tree load failures, "no
    /// session", …), keyed by the tab's id. Empty = nothing to say.
    pub rp_err_map: RwSignal<std::collections::HashMap<u32, String>>,
    /// M11: per-terminal-tab liveness (lamp + exit overlay), keyed by
    /// the terminal tab id (== the server pty id). A running=false
    /// after the shell used to run drives that tab's "exited — restart"
    /// overlay.
    pub term_state: RwSignal<std::collections::HashMap<u32, TermState>>,
    /// v0.5.42: model settings panel visibility.
    pub model_open: RwSignal<bool>,
    /// The server's model snapshot (GET /api/model).
    pub model_view: RwSignal<Option<ModelSettingsView>>,
    /// Panel error line (load / save failure).
    pub model_err: RwSignal<Option<String>>,
    /// Last save outcome — the banner that says what applied and what
    /// needs a loop restart.
    pub model_saved: RwSignal<Option<Value>>,
    /// Save in flight (disables the buttons).
    pub model_busy: RwSignal<bool>,
    /// Probe results keyed by entry name (the "test connection" line).
    pub model_probe: RwSignal<std::collections::HashMap<String, Value>>,
    /// v0.5.44: the configured model entry names — the options in the
    /// session card's model popup. Refreshed at mount and after a panel
    /// save.
    pub model_names: RwSignal<Vec<String>>,
    /// v0.5.46: entry name -> `context_tokens`, the budget the context bar
    /// reports for the active session's model. One `/api/model` fetch
    /// feeds this together with `model_names`.
    pub model_ctx: RwSignal<std::collections::BTreeMap<String, u64>>,
    /// Rewind plugin: the active session's projected history tree
    /// (`GET /api/sessions/{id}/rewind`). None until the first fetch.
    pub rewind_tree: RwSignal<Option<RewindTree>>,
    /// Rewind plugin: the confirm dialog's pending target (None = closed).
    pub rewind_pending: RwSignal<Option<RewindTarget>>,
    /// Rewind plugin: bumped when a `user_message` / `rewind` event arrives
    /// (the only structure-changing event types) so the tree refetches.
    pub rewind_gen: RwSignal<u64>,
    /// v0.5.57: display aliases for the session groups, keyed by the session's
    /// **working path** (`cwd`). The group head shows the path's basename or,
    /// when the user renamed it, this label — a display-only preference
    /// (localStorage `rushi-project-labels`); the path itself never changes.
    /// Shared by the sidebar dispatch view and the rewind History rail.
    pub project_labels: RwSignal<std::collections::HashMap<String, String>>,
    /// v0.5.57: which group head is being renamed (its path), None = none.
    pub group_edit: RwSignal<Option<String>>,
}

// v0.5.23: module-level handle to the live AppState (set once at app
// mount by lib.rs via `AppState::set_app_state`).
thread_local! {
    static APP_STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

impl AppState {
    pub fn new() -> Self {
        Self {
            sessions: RwSignal::new(Vec::new()),
            active_session: RwSignal::new(None),
            events: RwSignal::new(Vec::new()),
            ev_gen: RwSignal::new(0),
            last_ext: RwSignal::new(std::collections::HashMap::new()),
            cmd_gen: RwSignal::new(0),
            hist_oldest_line: RwSignal::new(0),
            hist_has_more: RwSignal::new(false),
            loading_earlier: RwSignal::new(false),
            hist_total_rounds: RwSignal::new(0),
            earlier_loaded: RwSignal::new(0),
            earlier_failed: RwSignal::new(false),
            ws_status: RwSignal::new("disconnected".into()),
            loop_running: RwSignal::new(false),
            ctx_used: RwSignal::new(0),
            goal: RwSignal::new(None),
            time_inject: RwSignal::new(None),
            active_plugin: RwSignal::new("goal".to_string()),
            plugin_menu_open: RwSignal::new(false),
            view_round: RwSignal::new(None),
            layout_mode: RwSignal::new("split".to_string()),
            menu_session: RwSignal::new(None),
            menu_pos: RwSignal::new((0.0, 0.0)),
            rounds_ctxk: RwSignal::new(Vec::new()),
            pile_count: RwSignal::new(0),
            pile_top_brief: RwSignal::new(String::new()),
            pile_open: RwSignal::new(false),
            new_session_open: RwSignal::new(None),
            confirm_delete: RwSignal::new(None),
            live_text: RwSignal::new(String::new()),
            live_reasoning: RwSignal::new(String::new()),
            streaming: RwSignal::new(false),
            settling_card: RwSignal::new(false),
            tool_pending: RwSignal::new(Vec::new()),
            round_active: RwSignal::new(None),
            looping_sessions: RwSignal::new(std::collections::HashSet::new()),
            loop_done_unviewed: RwSignal::new(std::collections::HashSet::new()),
            loop_viewed: RwSignal::new(std::collections::HashSet::new()),
            theme_mode: RwSignal::new(String::from("auto")),
            sort_mode: RwSignal::new(String::from("output")),
            custom_order: RwSignal::new(Vec::new()),
            output_rank: RwSignal::new(std::collections::HashMap::new()),
            dragging_session: RwSignal::new(None),
            drop_target: RwSignal::new(None),
            loop_cmd: RwSignal::new(String::new()),
            loop_cmd_sess: RwSignal::new(None),
            rp_open: RwSignal::new(false),
            // M13: start with NO tabs — the panel shows the "start"
            // view (Files / Terminal launcher buttons); tabs are
            // created on demand and are connection-scoped (a session
            // switch resets the panel to the empty start view).
            rp_tabs: RwSignal::new(Vec::new()),
            rp_active: RwSignal::new(0),
            rp_next_id: RwSignal::new(1),
            rp_term_seq: RwSignal::new(1),
            rp_expanded: RwSignal::new(std::collections::HashSet::new()),
            rp_children: RwSignal::new(std::collections::HashMap::new()),
            rp_preview_map: RwSignal::new(std::collections::HashMap::new()),
            rp_err_map: RwSignal::new(std::collections::HashMap::new()),
            term_state: RwSignal::new(std::collections::HashMap::new()),
            model_open: RwSignal::new(false),
            model_view: RwSignal::new(None),
            model_err: RwSignal::new(None),
            model_saved: RwSignal::new(None),
            model_busy: RwSignal::new(false),
            model_probe: RwSignal::new(std::collections::HashMap::new()),
            model_names: RwSignal::new(Vec::new()),
            model_ctx: RwSignal::new(std::collections::BTreeMap::new()),
            rewind_tree: RwSignal::new(None),
            rewind_pending: RwSignal::new(None),
            rewind_gen: RwSignal::new(0),
            project_labels: RwSignal::new(std::collections::HashMap::new()),
            group_edit: RwSignal::new(None),
        }
    }

    /// v0.5.46: publish a `/api/model` snapshot into the app state — the
    /// entry names behind the session-card popup and each entry's context
    /// budget behind the context bar. Called at mount (lib.rs) and after
    /// every settings-panel save/reload (ms.rs).
    pub fn set_model_settings(&self, v: &ModelSettingsView) {
        self.model_names
            .set(v.entries.iter().map(|e| e.name.clone()).collect());
        self.model_ctx.set(
            v.entries
                .iter()
                .filter_map(|e| e.context_tokens.map(|c| (e.name.clone(), c)))
                .collect(),
        );
    }

    /// v0.5.30: seed the "output" rank for sessions we have not stamped
    /// yet (first time observed this browser session): their rank
    /// starts from the server's `last_modified`. Existing stamps are
    /// NEVER overwritten here — the order only moves when a loop
    /// COMPLETES (`ws.rs` bumps `output_rank` on the `loop_status`
    /// running=false frame), which is what keeps the sidebar stable
    /// while several sessions stream concurrently.
    pub fn seed_output_rank(&self, sessions: &[SessionInfo]) {
        self.output_rank.update(|map| {
            for s in sessions {
                map.entry(s.name.clone())
                    .or_insert(s.last_modified.unwrap_or(0.0));
            }
        });
    }

    /// v0.5.30: keep `custom_order` aligned with the server's
    /// membership: drop deleted names, append new names at the end
    /// (in name order). Returns true when the order changed, so the
    /// caller can persist it.
    pub fn maintain_custom_order(&self, sessions: &[SessionInfo]) -> bool {
        // RwSignal::update's closure must return (), so the "did it
        // change?" answer is computed from before/after snapshots.
        let before = self.custom_order.get();
        let names: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.name.as_str()).collect();
        self.custom_order.update(|order| {
            order.retain(|n| names.contains(n.as_str()));
            for s in sessions.iter() {
                if !order.contains(&s.name) {
                    order.push(s.name.clone());
                }
            }
        });
        self.custom_order.get() != before
    }

    /// v0.5.30: both reconciliations after any session-list load
    /// (select / delete / the 10s poll / CLI-created sessions).
    /// Persist the custom order when it changed.
    pub fn sync_session_bookkeeping(&self, sessions: &[SessionInfo]) {
        self.seed_output_rank(sessions);
        if self.maintain_custom_order(sessions) {
            crate::ui::persist_custom_order(&self.custom_order.get());
        }
    }

    /// Clear all live-stream state: the in-progress model call's
    /// streamed text/reasoning, the streaming flag, and the
    /// running tool-call set. Called on session switch, active
    /// session deletion, and fresh WS connections.
    pub fn clear_live(&self) {
        self.live_text.set(String::new());
        self.live_reasoning.set(String::new());
        self.streaming.set(false);
        self.settling_card.set(false);
        self.tool_pending.set(Vec::new());
    }

    // v0.5.23: global handle to the live AppState (set once at app
    // mount by lib.rs). Lets out-of-tree diagnostics / the freeze
    // regression test reach the app's signals without re-creating
    // state. `AppState` is `Copy` (all `RwSignal` fields).
    pub fn set_app_state(s: AppState) {
        APP_STATE.with(|c| *c.borrow_mut() = Some(s));
    }

    pub fn current_app_state() -> Option<AppState> {
        APP_STATE.with(|c| *c.borrow())
    }

    /// Index range of events to show for the current view_round
    /// (live: all; Some(i): round i's events). Used by the Phase-4
    /// scroll/pile engine; allowed dead for now.
    #[allow(dead_code)]
    pub fn visible_range(&self) -> Round {
        // v0.5.52: everything under one borrow (was a whole-Vec clone
        // per call; unused today, but this is the same O(N) shape as the
        // freeze bug, §9.13).
        self.events.with(|ev| match self.view_round.get() {
            None => 0..ev.len(),
            Some(i) => compute_rounds(ev).get(i).cloned().unwrap_or(0..ev.len()),
        })
    }

    // ── v0.5.52: incremental derived state (plan §9.13/§9.14 P2) ────
    //
    // The status strip used to re-scan the whole event window (and
    // rebuild its chip views) on EVERY appended event, and the loop-cmd
    // chip re-derived itself per event too. Both are now maintained at
    // the write sites: O(1) per appended event, and a single pass when
    // the window is REPLACED (history frame / prepend) or cleared
    // (session switch).

    /// Apply one newly appended event to the derived signals.
    pub fn note_event(&self, ev: &Value) {
        note_event_signals(self.last_ext, self.cmd_gen, ev);
    }

    /// Recompute the derived signals in one pass over the whole window
    /// (call after `events.set(..)` / a prepend merge).
    pub fn rebuild_derived(&self) {
        rebuild_derived_signals(self.events, self.last_ext, self.cmd_gen);
    }

    /// Drop the derived signals (session switch / cleared window).
    pub fn clear_derived(&self) {
        self.last_ext.set(std::collections::HashMap::new());
        self.cmd_gen.update(|g| *g += 1);
    }
}

/// v0.5.52: `AppState::rebuild_derived` as a free function (see
/// `note_event_signals` for why: the WS closures must stay `'static`).
pub fn rebuild_derived_signals(
    events: RwSignal<Vec<Value>>,
    last_ext: RwSignal<std::collections::HashMap<String, String>>,
    cmd_gen: RwSignal<u64>,
) {
    let map = events.with(|v| {
        let mut m: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for ev in v.iter() {
            if ev.get("type").and_then(|t| t.as_str()) != Some("ext_status") {
                continue;
            }
            if let Some(id) = ev.get("id").and_then(|i| i.as_str()) {
                m.insert(id.to_string(), ext_status_value_str(ev.get("value")));
            }
        }
        m
    });
    last_ext.set(map);
    cmd_gen.update(|g| *g += 1);
}

/// v0.5.52: `AppState::note_event` as a free function taking the two
/// derived signals directly, so the hot append path can call it from a
/// closure that must stay `'static` (no `&AppState` capture).
pub fn note_event_signals(
    last_ext: RwSignal<std::collections::HashMap<String, String>>,
    cmd_gen: RwSignal<u64>,
    ev: &Value,
) {
    match ev.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "ext_status" => {
            let Some(id) = ev.get("id").and_then(|v| v.as_str()) else {
                return;
            };
            let val = ext_status_value_str(ev.get("value"));
            // Compare first: an unchanged value must not wake the strip
            // (a running hook re-reports the same id a lot).
            // untracked: called per live event from the WS task, outside
            // any reactive scope (a tracked read warned once per event).
            let same = last_ext.with_untracked(|m| m.get(id).map(|x| x == &val).unwrap_or(false));
            if !same {
                let id = id.to_string();
                last_ext.update(|m| {
                    m.insert(id, val);
                });
            }
        }
        "user_message" => cmd_gen.update(|g| *g += 1),
        _ => {}
    }
}

/// Render one `ext_status.value` the way the status strip shows it
/// (legacy briefValue parity: JSON for containers, `null` for null,
/// empty string when absent).
pub fn ext_status_value_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::Null) => "null".to_string(),
        Some(x @ Value::Object(_)) | Some(x @ Value::Array(_)) => x.to_string(),
        Some(x) => x.to_string(),
        None => String::new(),
    }
}
