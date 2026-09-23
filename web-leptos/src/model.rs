use leptos::prelude::*;
use serde::Deserialize;
use serde_json::Value;

/// One session row in the sidebar (port of JS `loadSessions`).
#[derive(Clone, Debug, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    #[serde(default)]
    pub last_modified: Option<f64>,
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

/// Shared reactive app state (port of the JS module-level globals).
#[derive(Clone, Copy)]
pub struct AppState {
    pub sessions: RwSignal<Vec<SessionInfo>>,
    pub active_session: RwSignal<Option<String>>,
    /// Normalized event stream (JSON values, matching the JS `events` array).
    pub events: RwSignal<Vec<Value>>,
    /// v0.5.17: 1-based line number of the OLDEST event currently in
    /// `events` (truncated history: the server ships only the last
    /// page on connect; 1 = the whole log is loaded). 0 = unknown.
    pub hist_oldest_line: RwSignal<u64>,
    /// v0.5.17: whether older events exist above the loaded window.
    pub hist_has_more: RwSignal<bool>,
    /// v0.5.17: true while a "load earlier" page request is in flight
    /// (guards against double-firing the button).
    pub loading_earlier: RwSignal<bool>,
    pub ws_status: RwSignal<String>,
    pub loop_running: RwSignal<bool>,
    pub ctx_used: RwSignal<u64>,
    pub goal: RwSignal<Option<GoalView>>,
    /// None = live view; Some(i) = show only round i (0-based).
    pub view_round: RwSignal<Option<usize>>,
    /// Sidebar collapse, persisted per browser.
    pub sidebar_collapsed: RwSignal<bool>,
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
}

impl AppState {
    pub fn new() -> Self {
        Self {
            sessions: RwSignal::new(Vec::new()),
            active_session: RwSignal::new(None),
            events: RwSignal::new(Vec::new()),
            hist_oldest_line: RwSignal::new(0),
            hist_has_more: RwSignal::new(false),
            loading_earlier: RwSignal::new(false),
            ws_status: RwSignal::new("disconnected".into()),
            loop_running: RwSignal::new(false),
            ctx_used: RwSignal::new(0),
            goal: RwSignal::new(None),
            view_round: RwSignal::new(None),
            sidebar_collapsed: RwSignal::new(false),
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

    /// Index range of events to show for the current view_round
    /// (live: all; Some(i): round i's events). Used by the Phase-4
    /// scroll/pile engine; allowed dead for now.
    #[allow(dead_code)]
    pub fn visible_range(&self) -> Round {
        let ev = self.events.get();
        match self.view_round.get() {
            None => 0..ev.len(),
            Some(i) => compute_rounds(&ev)
                .get(i)
                .cloned()
                .unwrap_or(0..ev.len()),
        }
    }
}
