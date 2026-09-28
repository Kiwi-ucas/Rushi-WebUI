use std::cell::RefCell;

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::Value;

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
            theme_mode: RwSignal::new(String::from("auto")),
            sort_mode: RwSignal::new(String::from("output")),
            custom_order: RwSignal::new(Vec::new()),
            output_rank: RwSignal::new(std::collections::HashMap::new()),
            dragging_session: RwSignal::new(None),
            drop_target: RwSignal::new(None),
            loop_cmd: RwSignal::new(String::new()),
            loop_cmd_sess: RwSignal::new(None),
        }
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
