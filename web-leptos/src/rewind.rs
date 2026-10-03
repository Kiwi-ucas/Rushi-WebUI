//! Rewind plugin (client): the conversation history tree.
//!
//! One data source (`GET /api/sessions/{id}/rewind`) feeds two surfaces:
//!   * the **full-window History view** — the rebuilt `layout-full` layout:
//!     a session rail on the left (the dispatch-view session cards, grouped
//!     by working path, with their loop start/stop toggle), the recursive
//!     round/branch tree on the right, one node per user message (one loop
//!     round);
//!   * the **sidebar plugin panel** (`#plugin-area`, registered in
//!     `plugins.rs`) — a compact summary + the active path.
//!
//! A node click opens [`RewindConfirmDialog`] ("Rewind to this point?") and,
//! on confirm, writes `rewind { target_seq, mode:"on" }`: the target user
//! message stays the active tail, everything after it moves to an abandoned
//! branch that remains in the append-only log — and in the tree — and can be
//! re-entered by another rewind.
//!
//! Rewind is **forbidden while the session's loop is running**: the card
//! quick button is disabled and the tree stays viewable but not clickable
//! ([`rewind_allowed`]).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, MouseEvent, PointerEvent};

use crate::api;
use std::collections::HashMap;

use crate::model::{
    AppState, FlowBranch, FlowNode, Restore, RewindNode, RewindTarget, SessionInfo,
};
use crate::timeutil;

/// Whether a rewind targeting `session` is allowed right now. Never while
/// that session's loop runs: an in-flight turn would land inside the fresh
/// branch, so the action is refused until the loop settles.
pub fn rewind_allowed(state: AppState, session: &str) -> bool {
    !(state.loop_running.get() || state.looping_sessions.get().contains(session))
}

/// Open the rewind confirm dialog for one target.
pub fn request_rewind(state: AppState, seq: u64, label: String) {
    state.rewind_pending.set(Some(RewindTarget { seq, label }));
}

/// One tree fetch for the whole app, keyed on the active session and the
/// structure-generation counter (`ws.rs` bumps it on `user_message` /
/// `rewind` frames only). Both surfaces read `state.rewind_tree`.
pub fn register_tree_effect(state: AppState) {
    let sess = state.active_session;
    let gen = state.rewind_gen;
    let tree = state.rewind_tree;
    Effect::new(move || {
        let Some(s) = sess.get() else {
            tree.set(None);
            return;
        };
        let _ = gen.get();
        spawn_local(async move {
            match api::load_rewind_tree(&s).await {
                Ok(t) => tree.set(Some(t)),
                // A failed fetch/parse used to vanish into the "loading…"
                // placeholder; keep the reason reachable (it is how the
                // v0.5.65 `missing`-less payload was found).
                Err(e) => {
                    web_sys::console::warn_1(&format!("rewind tree ({s}): {e}").into());
                    tree.set(None);
                }
            }
        });
    });
}

// ── Style B (the "flow" view): which style, and what is selected ───
//
// v0.5.67: the flow style is the default; the list is opt-in from the
// History top bar and the choice survives a reload. D5: opening it selects
// the round the session is at, so the detail panel is never empty. The
// selection is client-only and lives with the rest of the plugin state
// (`AppState::rw_selected`); clicking a node in the scene only sets it —
// the Rewind button in the detail panel is the only trigger there.

/// The localStorage key holding the History style ("tree" | "flow").
const VIEW_KEY: &str = "rushi-rw-view";

/// The persisted style: `"flow"` or `"tree"`. **`flow` is the default**
/// (user decision 2026-10-03, superseding D2's "list is the default"): the
/// scene draws every session measured, while the recursive list cannot
/// render the long ones at all (see `nodes_view`). Anything unknown falls
/// back to the flow style.
pub fn read_view_mode() -> String {
    let stored = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item(VIEW_KEY).ok())
        .flatten();
    match stored.as_deref() {
        Some("tree") => "tree".to_string(),
        _ => "flow".to_string(),
    }
}

/// Switch style and remember it (a display-only preference: no server call).
pub fn set_view_mode(state: AppState, mode: &str) {
    state.rw_view.set(mode.to_string());
    if let Some(s) = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
    {
        let _ = s.set_item(VIEW_KEY, mode);
    }
}

/// Select a round in the flow view: the detail panel follows, and **no
/// rewind happens** (the user's rule for the new style).
pub fn select_node(state: AppState, seq: u64) {
    state.rw_selected.set(Some(seq));
}

thread_local! {
    /// The one `resize` listener Style B installs. A resize is a layout
    /// point (not a scroll frame), so the scene re-measures itself here.
    static SCENE_RESIZE: RefCell<Option<Closure<dyn Fn()>>> = const { RefCell::new(None) };
    /// **v0.5.69 (round-3 defect 2)**: the turn a gesture could not spend
    /// on panning — see [`pan_and_turn`]. `--rw-scroll` is
    /// `scrollLeft + this`, so a scene narrower than its panel (which is
    /// what a track of 17..45 columns is, `#rw-flow-track`'s `--cell`
    /// makes it fit exactly) still turns the ring even though it has no
    /// scroll range at all. Reset at every layout, like `--rw0`.
    static RW_TURN: Cell<f64> = const { Cell::new(0.0) };
}

/// The scene's turn accumulator (px, the same unit as `scrollLeft`).
fn turn() -> f64 {
    RW_TURN.with(|c| c.get())
}
fn add_turn(d: f64) {
    RW_TURN.with(|c| c.set(c.get() + d));
}
fn set_turn(v: f64) {
    RW_TURN.with(|c| c.set(v));
}

/// B7's numbers, written once per **layout** so that a scroll frame writes
/// exactly one value.
///
/// Each ribbon's `--turn` is a pure CSS `calc()` over three plane numbers —
/// its own centre (`--bx`), the offset (`--rw-scroll`, the only thing a
/// scroll frame touches) and half the viewport plus the falloff width
/// (`--halfpw`, `--denom`). Numbers, never lengths: CSS cannot divide a
/// length by a length, and this way the proxy costs one `setProperty` per
/// scroll event and never reads layout while scrolling.
/// **v0.5.68 (D-orb-1, user decision)**: where the trunk sits in the scene,
/// as a fraction of its height. The ring is drawn *above* the trunk (the
/// bounded arc of D-orb-2 lives on the upper half), so the axis is below the
/// middle and the fins get the room.
const SCENE_AXIS_FRAC: f64 = 0.62;
/// Half the fin band in px (the CSS gives `.rw-fin` a 26px band).
const FIN_HALF_PX: f64 = 13.0;
/// Air between the farthest fin's end and the panel's edge.
const FIN_MARGIN_PX: f64 = 16.0;
/// A ring smaller than this reads as a squashed line; below it the fit gives
/// way rather than the geometry.
const FIN_MIN_RADIUS_PX: f64 = 34.0;
/// **D-orb-8**: one full turn per 1.5 scene widths of scrolling — a fixed
/// angular rate, so the gesture feels the same in every session.
const ORBIT_DEG_PER_WIDTH: f64 = 360.0 / 1.5;

fn sync_scene_metrics() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(scroll) = doc
        .query_selector("#rw-flow-scroll")
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
    else {
        return;
    };
    let w = scroll.client_width().max(1) as f64;
    let h = scroll.client_height().max(1) as f64;
    let half = w / 2.0;
    let st = scroll.style();
    let _ = st.set_property("--halfpw", &format!("{half}"));
    let _ = st.set_property("--denom", &format!("{}", half * 1.1));
    // the same value `mark_scroll` writes, turn included: a re-measure must
    // never move the ring
    mark_scroll(&scroll);

    // ── the orbital scene's pixels (the server owns the angles) ─────
    // The axis, and the radius the auto-fit gives it: the farthest fin is
    // the one at the front of the arc (its projected row is `-R`, z = 0, so
    // the perspective does not stretch it), and its end is half a fin above
    // that. `R = axis - margin - band/2` puts it exactly inside the panel —
    // D-orb-1 asked for the far end to *just* fit.
    let axis = (h * SCENE_AXIS_FRAC).round();
    let r = (axis - FIN_MARGIN_PX - FIN_HALF_PX).max(FIN_MIN_RADIUS_PX);
    let _ = st.set_property("--axis", &format!("{axis}px"));
    // px, not a bare number: the CSS reads it as a length
    let _ = st.set_property("--r", &format!("{r}px"));
    // ── v0.5.70: the cone's slope (plan §13, D-cone-2) ───────────────
    // Every branch shares one slope, and the *longest* branch sets it: its
    // farthest bead is the scene's highest point, so `q = span / longest`
    // puts that bead exactly inside the panel — the same auto-fit the ring's
    // radius had, per step instead of per branch. The length is read back off
    // the DOM (each container carries `data-n`), so the slope stays a
    // layout-time pixel value and the CSS keeps owning the cell.
    let longest = doc
        .query_selector_all(".rw-branch")
        .ok()
        .map(|list| {
            (0..list.length())
                .filter_map(|i| list.item(i))
                .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
                .filter_map(|el| el.get_attribute("data-n"))
                .filter_map(|s| s.parse::<f64>().ok())
                .fold(0.0_f64, f64::max)
        })
        .unwrap_or(0.0);
    let q = if longest > 0.0 { r / longest } else { 0.0 };
    let _ = st.set_property("--q", &format!("{q:.3}px"));
    // ── v0.5.70: the cell, in plain px, for the spine ───────────────
    // The spine's angle is `atan2(steps·q, columns·cell)` — and Chromium
    // refuses a *container unit* inside `atan2()` (measured: `--cell` is
    // `clamp(26px, calc(100cqw / N - 4px), 64px)`, which `hypot()` accepts
    // but `atan2()` treats as invalid, leaving the bar unrotated). So the
    // layout hands the CSS a plain length, *measured* off a segment that
    // already carries the real cell — never recomputed from a formula the
    // stylesheet would have to agree with.
    let cell_px = doc
        .query_selector(".rw3-seg")
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::Element>().ok())
        .and_then(|e| {
            web_sys::window()
                .and_then(|w| w.get_computed_style(&e).ok())
                .flatten()
        })
        .and_then(|cs| cs.get_property_value("width").ok())
        .and_then(|w| w.trim().trim_end_matches("px").parse::<f64>().ok());
    match cell_px {
        Some(c) => {
            let _ = st.set_property("--cellpx", &format!("{c:.3}px"));
        }
        None => {
            let _ = st.remove_property("--cellpx");
        }
    }
    // D-orb-8: a fixed angular rate, and `--rw0` = the offset the scene is at
    // when the layout runs, so entering the scene (or picking a round, or a
    // new round arriving) puts the ring back at rest with the aligned fin in
    // front — "the alignment rule always wins at rest".
    let _ = st.set_property("--dpp", &format!("{}", ORBIT_DEG_PER_WIDTH / w));
    let _ = st.set_property("--rw0", &format!("{}", scroll.scroll_left()));
}

/// A **layout point** (v0.5.69): re-measure the scene and put the ring back
/// at rest. The turn resets with `--rw0`, so what the user sees on entering
/// the view, on picking a round, or on a resize is the aligned fin at the
/// front (the D-orb-7 rule). A bare [`sync_scene_metrics`] is only a
/// re-measure (`on_down`) and must leave the ring exactly where it is.
fn relayout_scene() {
    set_turn(0.0);
    sync_scene_metrics();
}

/// Style B's two effects, registered once from `lib.rs` next to
/// [`register_tree_effect`]:
///
/// 1. keep a valid selection — reset on a session switch, then default to
///    the current round (D5); a round the user picked stays picked across
///    tree reloads (a new round arriving must not steal the panel);
/// 2. fetch the selected round in full (B2) whenever the selection changes
///    — and only while the flow view is showing, so style A makes no
///    useless request.
pub fn register_flow_effects(state: AppState) {
    let sess = state.active_session;
    let tree = state.rewind_tree;
    let view = state.rw_view;
    let selected = state.rw_selected;
    let detail = state.rw_detail;

    // B7: a new scene (or a resized panel) re-measures the ribbons. The
    // first pass runs after the render that produced them.
    Effect::new(move || {
        let _ = (view.get(), tree.get(), selected.get());
        relayout_scene();
        gloo_timers::callback::Timeout::new(0, relayout_scene).forget();
    });
    SCENE_RESIZE.with(|slot| {
        if slot.borrow().is_some() {
            return;
        }
        let cb = Closure::<dyn Fn()>::new(relayout_scene);
        if let Some(w) = web_sys::window() {
            let _ = w.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        }
        *slot.borrow_mut() = Some(cb);
    });

    let last_sess: StoredValue<Option<String>> = StoredValue::new(None);
    Effect::new(move || {
        let s = sess.get();
        if last_sess.get_value() != s {
            last_sess.set_value(s);
            selected.set(None);
            detail.set(None);
        }
        if view.get() != "flow" {
            return;
        }
        let Some(t) = tree.get() else { return };
        let known = selected
            .get()
            .is_some_and(|seq| t.flow.nodes.iter().any(|n| n.seq == seq));
        if !known {
            selected.set(t.current_seq);
        }
    });

    Effect::new(move || {
        if view.get() != "flow" {
            return;
        }
        let (Some(seq), Some(s)) = (selected.get(), sess.get()) else {
            detail.set(None);
            return;
        };
        spawn_local(async move {
            match api::load_rewind_detail(&s, seq).await {
                Ok(d) => detail.set(Some(d)),
                Err(e) => {
                    web_sys::console::warn_1(&format!("rewind round {seq} ({s}): {e}").into());
                    detail.set(None);
                }
            }
        });
    });
}

/// Display label of a node ("round 3 · first chars…").
pub fn node_label(n: &RewindNode) -> String {
    format!("round {} \u{00b7} {}", n.round, n.summary)
}

// ── the full-window History view (the rebuilt layout-full) ─────────

/// The new full-window interface: top bar, session rail, tree, footer hint.
pub fn history_view(state: AppState) -> AnyView {
    let active = state.active_session;
    let layout = state.layout_mode;
    let theme_mode = state.theme_mode;

    view! {
        <div id="history-view">
            <div id="hist-top">
                <button
                    id="hist-back"
                    title="back to the chat"
                    on:click=move |_| {
                        crate::ui::set_layout_mode("split");
                        layout.set("split".to_string());
                    }
                >
                    { "\u{2039} back to chat" }
                </button>
                <div id="hist-title">
                    <span class="hist-head">{ "history" }</span>
                    <span class="hist-sess">
                        { move || active.get().unwrap_or_else(|| "no session".to_string()) }
                    </span>
                </div>
                <span
                    class=move || {
                        let running = active
                            .get()
                            .map(|s| {
                                state.loop_running.get()
                                    || state.looping_sessions.get().contains(s.as_str())
                            })
                            .unwrap_or(false);
                        if running { "rw-lamp running" } else { "rw-lamp" }
                    }
                    title=move || {
                        let running = active
                            .get()
                            .map(|s| {
                                state.loop_running.get()
                                    || state.looping_sessions.get().contains(s.as_str())
                            })
                            .unwrap_or(false);
                        if running {
                            "the loop is running \u{2014} rewind is disabled".to_string()
                        } else {
                            "the loop is idle".to_string()
                        }
                    }
                ></span>
                { hist_style_switch(state) }
                <button
                    id="hist-theme"
                    title=move || match theme_mode.get().as_str() {
                        "light" => "theme: light \u{2014} click for dark".to_string(),
                        "dark" => "theme: dark \u{2014} click for auto".to_string(),
                        _ => "theme: auto (follows system) \u{2014} click for light".to_string(),
                    }
                    on:click=move |_| {
                        let next = match theme_mode.get().as_str() {
                            "auto" => "light",
                            "light" => "dark",
                            _ => "auto",
                        };
                        theme_mode.set(next.to_string());
                        crate::ui::set_theme_mode_stored(next);
                        crate::ui::theme_apply(state);
                    }
                >
                    { move || crate::ui::theme_icon(theme_mode.get().as_str()) }
                </button>
            </div>
            <div id="hist-body">
                <div id="hist-rail">{ hist_rail(state) }</div>
                <Show
                    when=move || state.rw_view.get() == "flow"
                    fallback=move || view! { <div id="hist-tree">{ hist_tree(state) }</div> }
                >
                    { flow_view(state) }
                </Show>
            </div>
            <div id="hist-foot">{ move || hist_hint(state) }</div>
        </div>
    }
    .into_any()
}

/// Footer hint: what a click does, or why it is locked.
fn hist_hint(state: AppState) -> String {
    let Some(sess) = state.active_session.get() else {
        return "select a session".to_string();
    };
    if !rewind_allowed(state, &sess) {
        "rewind is disabled while the loop is running".to_string()
    } else {
        "click a node to rewind \u{00b7} disabled while the loop runs".to_string()
    }
}

/// The session rail: the dispatch-view **session cards, grouped by working
/// path** (v0.5.56, the C2 decision keeps the rail inside the full-window
/// view). It reuses the sidebar's card wholesale — `ui::session_card` (name,
/// last-output time, the per-session `\u{25B6} start` / `\u{25A0} stop` loop
/// toggle, the `\u{2026}` rename/delete menu) and `ui::session_group_head`
/// (the project directory + count) — with `stay = true`, so clicking a card
/// switches the tree's session in place instead of dropping out of the
/// History view.
///
/// The grouping key is the session's `cwd` marker (its working directory),
/// i.e. one group per project; sessions without a marker land in
/// "(no project)".
fn hist_rail(state: AppState) -> AnyView {
    let sessions = state.sessions;
    let sort_mode = state.sort_mode;
    let custom_order = state.custom_order;
    let output_rank = state.output_rank;

    view! {
        <For
            each=move || {
                crate::model::dispatch_groups(
                    &sessions.get(),
                    &sort_mode.get(),
                    &custom_order.get(),
                    &output_rank.get(),
                )
            }
            key=|g: &(String, Vec<SessionInfo>)| {
                let mut k = g.0.clone();
                k.push_str("::");
                for s in &g.1 {
                    k.push_str(&s.name);
                    k.push(',');
                }
                k
            }
            children=move |g: (String, Vec<SessionInfo>)| {
                let key = g.0.clone();
                let items = g.1;
                let count = items.len();
                view! {
                    <div class="dispatch-group">
                        { crate::ui::session_group_head(state, &key, count) }
                        <For
                            each=move || items.clone()
                            key=|s: &SessionInfo| s.name.clone()
                            children=move |s: SessionInfo| {
                                crate::ui::session_card(state, s, true)
                            }
                        />
                    </div>
                }
            }
        />
    }
    .into_any()
}

/// The tree body: legend, recursive nodes, boundary footnotes.
fn hist_tree(state: AppState) -> AnyView {
    let tree = state.rewind_tree;
    let active = state.active_session;
    view! {
        { move || match (active.get(), tree.get()) {
            // No session selected yet (a fresh page load, before any card).
            (None, _) => view! { <div class="rw-empty">{ "select a session" }</div> }.into_any(),
            (_, None) => view! { <div class="rw-empty">{ "loading\u{2026}" }</div> }.into_any(),
            (_, Some(t)) if t.total_rounds == 0 => view! {
                <div class="rw-empty">
                    { "no rounds yet \u{2014} send a message to start the history." }
                </div>
            }.into_any(),
            (_, Some(t)) => {
                let sess = t.session.clone();
                let nodes = t.roots.clone();
                let abandoned = t.abandoned_rounds();
                let rewinds = t.rewinds.len();
                let boundaries = t.boundaries.clone();
                let boundaries_view: AnyView = if boundaries.is_empty() {
                    ().into_any()
                } else {
                    view! {
                        <div class="rw-boundaries">
                            <For
                                each=move || boundaries.clone()
                                key=|b: &crate::model::RewindBoundary| b.seq
                                children=move |b: crate::model::RewindBoundary| {
                                    view! {
                                        <div class="rw-boundary">
                                            { format!("compaction boundary at line {} \u{2014} rewinds older than it land at the boundary", b.seq) }
                                        </div>
                                    }
                                }
                            />
                        </div>
                    }
                    .into_any()
                };
                view! {
                    <div class="rw-legend">
                        <span class="rw-lg on">{ "active" }</span>
                        <span class="rw-lg off">{ format!("abandoned ({abandoned})") }</span>
                        <span class="rw-lg fork">{ format!("{rewinds} rewind{}", if rewinds == 1 { "" } else { "s" }) }</span>
                    </div>
                    <div class="rw-nodes">
                        { nodes_view(state, sess, nodes) }
                    </div>
                    { boundaries_view }
                }.into_any()
            }
        } }
    }
    .into_any()
}

/// One DOM level per **fork** (v0.5.67).
///
/// The list used to nest one `children` container per round, so a straight
/// chain of N rounds cost ~2N DOM levels and the wasm stack gave out
/// somewhere past ~32–80 rounds: `Webui` (117) and `alpha` (82) drew six
/// nodes and threw `RuntimeError: memory access out of bounds` (the app
/// survived, the tree never painted). A node with a **single** child now
/// continues as a sibling in the same container, so the recursion depth is
/// the number of *forks* along the path instead of the number of rounds —
/// and a pure chain draws as one flat list rather than a staircase.
fn nodes_view(state: AppState, sess: String, nodes: Vec<RewindNode>) -> Vec<AnyView> {
    let mut out: Vec<AnyView> = Vec::with_capacity(nodes.len());
    let mut queue: VecDeque<RewindNode> = nodes.into();
    while let Some(mut n) = queue.pop_front() {
        if n.children.len() == 1 {
            // the run continues: hoist the only child to be the next sibling
            if let Some(child) = n.children.pop() {
                queue.push_front(child);
            }
        }
        out.push(node_view(state, sess.clone(), n));
    }
    out
}

/// One round node + its fork children (via [`nodes_view`]). Abandoned
/// branches keep their dimmed styling; the current one carries the "here"
/// marker.
fn node_view(state: AppState, sess: String, node: RewindNode) -> AnyView {
    let seq = node.seq;
    let round = node.round;
    let summary = node.summary.clone();
    let events = node.events;
    let ts = timeutil::ts_full(&node.ts);
    let abandoned = node.state != "active";
    let current = node.current;
    let retracted = node.retracted;
    let kids = node.children.clone();
    let restore = node.restore.clone();
    let blocked = restore.blocked();
    let restore_label = restore.label();

    let sess_cls = sess.clone();
    let node_cls = move || {
        let mut c = String::from("rw-node");
        if abandoned {
            c.push_str(" abandoned");
        }
        if current {
            c.push_str(" current");
        }
        if !rewind_allowed(state, &sess_cls) {
            c.push_str(" locked");
        }
        if blocked {
            c.push_str(" unresumable");
        }
        c
    };
    let sess_title = sess.clone();
    let ts_title = ts.clone();
    let state_title = node.state.clone();
    let restore_title = restore_label.clone();
    let title = move || {
        let head = format!(
            "round {round} \u{00b7} {ts_title} \u{00b7} {events} event{} \u{00b7} {state_title}",
            if events == 1 { "" } else { "s" }
        );
        if !rewind_allowed(state, &sess_title) {
            format!("{head} \u{2014} rewind is disabled while the loop is running")
        } else if blocked {
            format!("{head} \u{2014} this point cannot be resumed: it would strand a tool call")
        } else if current {
            format!("{head} \u{2014} you are here \u{00b7} resumes from {restore_title}")
        } else {
            format!(
                "{head} \u{2014} resumes from {restore_title} \u{2014} click to rewind to this point"
            )
        }
    };
    let label = format!("round {round} \u{00b7} {summary}");
    let sess_click = sess.clone();
    let on_click = move |_| {
        if current || !rewind_allowed(state, &sess_click) {
            return;
        }
        request_rewind(state, seq, label.clone());
    };

    let badge: AnyView = if retracted {
        view! { <span class="rw-badge-retract">{ "retracted" }</span> }.into_any()
    } else {
        ().into_any()
    };
    let warn: AnyView = if blocked {
        view! { <span class="rw-badge-warn">{ "not resumable" }</span> }.into_any()
    } else {
        ().into_any()
    };
    let restore_text = restore_label.clone();

    let kids_view: AnyView = if kids.is_empty() {
        ().into_any()
    } else {
        let sess_kids = sess.clone();
        view! { <div class="rw-kids">{ nodes_view(state, sess_kids, kids) }</div> }.into_any()
    };

    view! {
        <div class=node_cls data-seq=seq.to_string() data-round=round.to_string()>
            <div class="rw-head" title=title on:click=on_click>
                <span class="rw-dot"></span>
                <span class="rw-round">{ format!("round {round}") }</span>
                <span class="rw-sum">{ summary.clone() }</span>
                { badge }
                { warn }
                <span class="rw-meta">
                    { format!("{events} event{}", if events == 1 { "" } else { "s" }) }
                </span>
                <span class="rw-restore">{ restore_text }</span>
                <span class="rw-time">{ ts.clone() }</span>
                <Show when=move || current fallback=|| ()>
                    <span class="rw-here">{ "here" }</span>
                </Show>
            </div>
            { kids_view }
        </div>
    }
    .into_any()
}

// ── Style B: the "flow" view (plan section 10, steps B4-B8) ───────
//
// The History view's second style. Style A (the recursive list) is
// untouched but no longer the default (v0.5.67); this one is the default
// and remembers the choice.
//
// The right-hand side splits 1 : 2 (D3): the **detail panel** on top (the
// selected round in full + the Rewind button, the only place a rewind can
// be triggered here) and the **scene** below — the longest chain drawn as
// a straight horizontal `. - . - .` line with every other branch forking
// into a lane above or below it (§10.2, computed server-side).
//
// Two rules from the user shape the interaction: clicking a node only
// **selects** it (no dialog, no write — `select_node`), and the current
// round is pre-selected so the panel is never empty (D5).

/// The `[ list | flow ]` switch in the History top bar. Labelled for the
/// user ("list" is the style they know), keyed for the code (`tree`).
fn hist_style_switch(state: AppState) -> AnyView {
    let view = state.rw_view;
    let opt = move |mode: &'static str, label: &'static str, hint: &'static str| {
        let on = {
            let view = view;
            move || view.get() == mode
        };
        view! {
            <button
                class="hs-opt"
                class:on=on
                data-view=mode
                title=hint
                on:click=move |_| set_view_mode(state, mode)
            >
                { label }
            </button>
        }
    };
    view! {
        <div id="hist-style" role="group" title="history style">
            { opt("tree", "list", "the round list \u{2014} click a round to rewind") }
            { opt("flow", "flow", "the flow graph \u{2014} click a round to inspect it") }
        </div>
    }
    .into_any()
}

/// Style B's right-hand side: the 1:2 split (detail over scene).
fn flow_view(state: AppState) -> AnyView {
    view! {
        <div id="rw-split">
            <div id="rw-detail">{ flow_detail(state) }</div>
            <div id="rw-flow">{ flow_scene(state) }</div>
        </div>
    }
    .into_any()
}

/// The detail panel: the selected round in full (B2's route), its
/// annotations, and the Rewind button (B8 — the only trigger in this
/// style, and disabled under the same guard as everywhere else).
fn flow_detail(state: AppState) -> AnyView {
    let selected = state.rw_selected;
    let detail = state.rw_detail;
    let tree = state.rewind_tree;

    view! {
        <Show
            when=move || selected.get().is_some()
            fallback=move || view! {
                <div class="rw-empty">{ "click a round below to inspect it" }</div>
            }
        >
            { move || {
                let Some(seq) = selected.get() else { return ().into_any() };
                // The panel's facts come from B2's per-round route; the flow
                // node is the fallback (and the only carrier of `restore`)
                // while the fetch is in flight.
                let d = detail.get().filter(|d| d.seq == seq);
                let node = tree
                    .get()
                    .and_then(|t| t.flow.nodes.iter().find(|n| n.seq == seq).cloned());
                let round = d
                    .as_ref()
                    .map(|d| d.round)
                    .or_else(|| node.as_ref().map(|n| n.round))
                    .unwrap_or_default();
                let events = d
                    .as_ref()
                    .map(|d| d.events)
                    .or_else(|| node.as_ref().map(|n| n.events))
                    .unwrap_or_default();
                let ts = d
                    .as_ref()
                    .map(|d| d.ts.clone())
                    .or_else(|| node.as_ref().map(|n| n.ts.clone()))
                    .unwrap_or_default();
                let abandoned = d
                    .as_ref()
                    .map(|d| d.state != "active")
                    .or_else(|| node.as_ref().map(|n| n.state != "active"))
                    .unwrap_or(false);
                let current = d
                    .as_ref()
                    .map(|d| d.current)
                    .or_else(|| node.as_ref().map(|n| n.current))
                    .unwrap_or(false);
                let retracted = d
                    .as_ref()
                    .map(|d| d.retracted)
                    .or_else(|| node.as_ref().map(|n| n.retracted))
                    .unwrap_or(false);
                let blocked = node.as_ref().is_some_and(|n| n.restore.blocked());
                let restore = node.as_ref().map(|n| n.restore.label()).unwrap_or_default();
                let text = d
                    .as_ref()
                    .map(|d| d.text.clone())
                    .or_else(|| node.as_ref().map(|n| n.summary.clone()))
                    .unwrap_or_default();
                let pending = d.is_none();
                let allowed = tree
                    .get()
                    .map(|t| rewind_allowed(state, &t.session))
                    .unwrap_or(false);
                let label = format!("round {round} \u{00b7} {}", summarize_label(&text));
                // the closure below owns one copy; the panel shows the other
                let restore_t = restore.clone();

                let badge = |cls: &'static str, text: &'static str| {
                    view! { <span class=cls>{ text }</span> }
                };
                let mut badges: Vec<AnyView> = Vec::new();
                if current {
                    badges.push(badge("rw3-badge here", "here").into_any());
                }
                if abandoned {
                    badges.push(badge("rw3-badge off", "abandoned").into_any());
                }
                if retracted {
                    badges.push(badge("rw3-badge retract", "retracted").into_any());
                }
                if blocked {
                    badges.push(badge("rw3-badge warn", "not resumable").into_any());
                }

                view! {
                    <div class="fd-head" data-pending=pending.to_string()>
                        <span class="fd-round">{ format!("round {round}") }</span>
                        <span class="fd-meta">
                            { format!(
                                "line {seq} \u{00b7} {} \u{00b7} {events} event{}",
                                timeutil::ts_full(&ts),
                                if events == 1 { "" } else { "s" },
                            ) }
                        </span>
                        <Show when=move || pending>
                            <span class="rw3-badge load">{ "loading\u{2026}" }</span>
                        </Show>
                        { badges }
                    </div>
                    <div class="fd-text">{ text }</div>
                    <div class="fd-foot">
                        <button
                            class="fd-rewind"
                            data-seq=seq.to_string()
                            disabled=move || !allowed || current || blocked
                            title=move || {
                                if !allowed {
                                    "rewind is disabled while the loop is running".to_string()
                                } else if current {
                                    "the session is already here".to_string()
                                } else if blocked {
                                    format!(
                                        "this point cannot be resumed: it would strand a tool call ({restore_t})"
                                    )
                                } else {
                                    format!("rewind to round {round} \u{00b7} {restore_t}")
                                }
                            }
                            on:click=move |_| {
                                if allowed && !current && !blocked {
                                    request_rewind(state, seq, label.clone());
                                }
                            }
                        >
                            { if current { "you are here" } else { "\u{21ba} Rewind to this point" } }
                        </button>
                        <span class="fd-restore">{ restore }</span>
                    </div>
                }
                .into_any()
            } }
        </Show>
    }
    .into_any()
}

/// The single value a scroll frame writes (B7): the offset the ribbons'
/// `--turn` calc reads. Never `scrollLeft` itself, so nothing reads layout
/// while scrolling, and `--halfpw`/`--denom`/`--bx` stay from the layout.
fn mark_scroll(el: &HtmlElement) {
    let v = el.scroll_left() as f64 + turn();
    let _ = el.style().set_property("--rw-scroll", &format!("{v}"));
}

/// **v0.5.69 (round-3 defect 2)**: move the scene by `delta` px along the
/// line — as far as the track allows — and keep the rest as *turn*, so
/// the ring's `--phase` keeps moving when the trunk cannot.
///
/// `--phase` reads `--rw-scroll` (the wheel = the line = the ring, one
/// value per frame, D7/D-orb-4). That only ever worked while the scene
/// could scroll: `#rw-flow-track` is `(cols+1)` cells wide and `--cell`
/// auto-fits down to a 26px floor, so a session of 17..45 rounds fits its
/// panel **exactly** — `scrollLeft` is pinned at 0 for ever and the ring
/// froze (the live `rewind` and `Time inject` sessions are both in that
/// band). Now the gesture pans first and turns afterwards: with a track
/// that can move, the pan takes the whole delta (nothing changes from
/// before) and only the overshoot at either end becomes turn.
fn pan_and_turn(el: &HtmlElement, delta: f64) {
    let before = el.scroll_left() as f64;
    el.set_scroll_left((before + delta) as i32);
    let after = el.scroll_left() as f64;
    add_turn((before + delta) - after);
    mark_scroll(el);
}

/// Short label for the dialog's title line ("round 3 \u{00b7} first chars\u{2026}").
fn summarize_label(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 60 {
        flat
    } else {
        let s: String = flat.chars().take(60).collect();
        format!("{s}\u{2026}")
    }
}

/// The scene: the straight main line, the branches forking off it, one
/// clickable dot per round. The player: the server's logical `(x, lane)`
/// paid for in px here, with the column pitch left to CSS (it auto-fits
/// the panel down to a floor, D4).
fn flow_scene(state: AppState) -> AnyView {
    let tree = state.rewind_tree;
    // the scene draws the ring's alignment from the selection (D-orb-7)
    let selected = state.rw_selected;
    view! {
        { move || {
            let Some(t) = tree.get() else {
                return view! { <div class="rw-empty">{ "loading\u{2026}" } </div> }.into_any();
            };
            if t.flow.nodes.is_empty() {
                return view! {
                    <div class="rw-empty">
                        { "no rounds yet \u{2014} send a message to start the history." }
                    </div>
                }
                .into_any();
            }
            let f = t.flow.clone();
            let cells = f.cols.max(1).to_string();
            let lanes = f.lanes.max(1).to_string();
            // the ring's angular step and the visible arc, from the server
            // (one source of truth for the constants, tested there)
            let step = f.orbit.step_deg.max(1).to_string();
            let arc = f.orbit.arc_deg.max(1).to_string();
            // Where each node sits, and whether it is on the live path: the
            // connectors are drawn from the parent's dot, and an edge is lit
            // by the child's state (D1 — the *path* is highlighted, so the
            // live context reads even when the current round is on a fork).
            let at: HashMap<u64, (u64, i32, bool)> = f
                .nodes
                .iter()
                .map(|n| (n.seq, (n.x, n.lane, n.state == "active")))
                .collect();

            // **v0.5.68**: which fin each round belongs to, server-grouped
            // (`branch.seqs` — a nested fork shares its parent's lane, so the
            // lane cannot group them), and which fin sits at the front at
            // rest. D-orb-7: the selected round's branch, else the current
            // round's, else the first — so the picture answers "where am I"
            // without reading a word.
            let fin_of: HashMap<u64, u32> = f
                .branches
                .iter()
                .filter_map(|b| b.fin.map(|i| (b, i)))
                .flat_map(|(b, i)| b.seqs.iter().map(move |s| (*s, i)))
                .collect();

            // The trunk: the main-line runs. They never turn — the branches
            // orbit around *them* (D-orb-3, user decision) — so the trunk's
            // rect is bit-identical before and after a scroll.
            let trunk_edges: Vec<AnyView> = f
                .edges
                .iter()
                .filter(|e| e.main)
                .map(|e| {
                    let (fx, _, _) = at.get(&e.from).copied().unwrap_or((0, 0, true));
                    let lit = at.get(&e.to).map(|t| t.2).unwrap_or(true);
                    view! {
                        <div
                            class=format!(
                                "rw3-seg main {}",
                                if lit { "active" } else { "abandoned" },
                            )
                            data-from=e.from.to_string()
                            data-to=e.to.to_string()
                            style=format!("--x:{fx}; --lane:0")
                        ></div>
                    }
                    .into_any()
                })
                .collect();

            let by_seq: HashMap<u64, FlowNode> =
                f.nodes.iter().map(|n| (n.seq, n.clone())).collect();

            // ── the branches are a *forest*, not a ring of siblings ─────            // v0.5.70 / D-cone-6/7: a branch leaves its parent's **bead**. A
            // trunk-parented branch hinges on the axis at its parent's column;
            // a branch whose parent is itself on a branch is rendered *inside*
            // its parent's container, so the parent's turn is already in force
            // and the child's own angle reads as a small fan (±1 slot)
            // relative to its parent. The relation comes from the reduced edge
            // (a branch root's parent), never from the lane — a nested fork
            // shares its parent's lane (v0.5.68).
            let by_fin: HashMap<u32, &FlowBranch> = f
                .branches
                .iter()
                .filter_map(|b| b.fin.map(|i| (i, b)))
                .collect();
            let parent_of: HashMap<u64, u64> = f
                .edges
                .iter()
                .filter(|e| !e.main)
                .map(|e| (e.to, e.from))
                .collect();
            let owner: HashMap<u32, Option<u32>> = by_fin
                .iter()
                .map(|(i, b)| {
                    let o = parent_of
                        .get(&b.root)
                        .and_then(|p| fin_of.get(p).copied());
                    (*i, o)
                })
                .collect();
            let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
            let mut roots: Vec<u32> = Vec::new();
            for (&i, o) in owner.iter() {
                match o {
                    Some(p) => kids.entry(*p).or_default().push(i),
                    None => roots.push(i),
                }
            }
            roots.sort_unstable();
            for v in kids.values_mut() {
                v.sort_unstable();
            }
            // The topmost trunk-parented ancestor of each fin: the one
            // `--align` turns to the front (D-orb-7) — which brings a nested
            // fan along with it, since a child's turn composes with its
            // parent's.
            let mut top: HashMap<u32, u32> = HashMap::new();
            for &i in &roots {
                top.insert(i, i);
            }
            for _ in 0..by_fin.len() {
                let known: Vec<(u32, u32)> = kids
                    .iter()
                    .filter_map(|(p, cs)| top.get(p).map(|t| (*t, cs.clone())))
                    .flat_map(|(t, cs)| cs.into_iter().map(move |c| (c, t)))
                    .collect();
                if known.is_empty() {
                    break;
                }
                for (c, t) in known {
                    top.insert(c, t);
                }
            }
            let align = selected
                .get()
                .and_then(|s| fin_of.get(&s).copied())
                .or_else(|| t.current_seq.and_then(|s| fin_of.get(&s).copied()))
                .and_then(|i| top.get(&i).copied())
                .unwrap_or_else(|| roots.first().copied().unwrap_or(0));

            let cone = Cone {
                state,
                at: &at,
                by_seq: &by_seq,
                by_fin: &by_fin,
                parent_of: &parent_of,
                kids: &kids,
                step: f.orbit.step_deg.max(1),
                arc: f.orbit.arc_deg.max(1),
            };
            let fins: Vec<AnyView> = roots
                .iter()
                .map(|&i| cone.branch(i, 0, 0, i as i32, i as i32, 0, 1.0, 0))
                .collect();
            // how many fins the ring actually has (D-orb-9 reads this)
            let fins_len = by_fin.len();

            let trunk_nodes: Vec<AnyView> = f
                .nodes
                .iter()
                .filter(|n| !fin_of.contains_key(&n.seq))
                .map(|n| flow_node_button(state, n.clone(), 0))
                .collect();

            // Drag the empty space to pan (D4/D7): the wheel drives the same
            // scroll offset natively (an x-only scroller takes the vertical
            // delta), the dots are excluded so a click is never eaten.
            let drag = StoredValue::new((false, 0.0, 0.0, 0.0));
            let el_of = |ev: &PointerEvent| -> Option<HtmlElement> {
                ev.current_target()?.dyn_into::<HtmlElement>().ok()
            };
            let on_down = move |ev: PointerEvent| {
                let Some(el) = el_of(&ev) else { return };
                // a drag starts after some layout, possibly: re-measure
                sync_scene_metrics();
                let on_node = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .and_then(|t| t.closest(".rw3-node").ok().flatten())
                    .is_some();
                if on_node {
                    return;
                }
                drag.set_value((
                    true,
                    ev.client_x() as f64,
                    el.scroll_left() as f64,
                    turn(),
                ));
                let _ = el.set_pointer_capture(ev.pointer_id());
                let _ = el.class_list().add_1("dragging");
            };
            let on_move = move |ev: PointerEvent| {
                let (down, x0, sl, t0) = drag.get_value();
                if !down {
                    return;
                }
                let Some(el) = el_of(&ev) else { return };
                let dx = ev.client_x() as f64 - x0;
                // The pointer's *absolute* wish (the drag is anchored at
                // the press), and the part of it the track refuses becomes
                // the turn — continuous, so dragging past the end keeps
                // turning the ring instead of stopping dead.
                let want = sl - dx;
                el.set_scroll_left(want as i32);
                let got = el.scroll_left() as f64;
                set_turn(t0 + (want - got));
                mark_scroll(&el);
            };
            let on_up = move |ev: PointerEvent| {
                if let Some(el) = el_of(&ev) {
                    let _ = el.release_pointer_capture(ev.pointer_id());
                    let _ = el.class_list().remove_1("dragging");
                }
                drag.set_value((false, 0.0, 0.0, 0.0));
            };
            // Any scroll (this is also the fallback for a scrollbar drag we
            // never see, a keyboard scroll, or a programmatic `scrollLeft`
            // — the probe's H7 found the stale-var case) refreshes the same
            // single value.
            let on_scroll = move |ev: web_sys::Event| {
                if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<HtmlElement>().ok())
                {
                    mark_scroll(&el);
                }
            };
            // D7: the plain wheel drives the line too. A horizontal delta
            // (or shift+wheel) is already this element's own scroll, so only
            // the vertical delta is taken over — the plan's "scroll proxy",
            // measured in pixels/lines/pages like the browser does.
            let on_wheel = move |ev: web_sys::WheelEvent| {
                let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<HtmlElement>().ok())
                else {
                    return;
                };
                let dy = ev.delta_y()
                    * match ev.delta_mode() {
                        1 => 16.0,
                        2 => el.client_height() as f64,
                        _ => 1.0,
                    };
                if dy.abs() < 0.5 {
                    return;
                }
                ev.prevent_default();
                pan_and_turn(&el, dy);
            };

            view! {
                <div
                    id="rw-flow-scroll"
                    on:pointerdown=on_down
                    on:pointermove=on_move
                    on:pointerup=on_up
                    on:pointercancel=on_up
                    on:wheel=on_wheel
                    on:scroll=on_scroll
                >
                    <div
                        id="rw-flow-track"
                        style=format!(
                            "--cols:{cells}; --lanes:{lanes}; --align:{align}; --step:{step}; \
                             --arc:{arc}",
                        )
                    >
                        <div class="rw3-grid">
                            { trunk_edges }
                            { trunk_nodes }
                            // D-orb-9: a lone branch is a *swing*, not a ring
                            // that can park — the class lets the stylesheet
                            // read the phase through a sine instead.
                            <div class={move || {
                                if fins_len == 1 { "rw-orbit solo" } else { "rw-orbit" }
                            }}>{ fins }</div>
                        </div>
                        <div class="rw3-sheen"></div>
                    </div>
                </div>
            }
            .into_any()
        } }
    }
    .into_any()
}

/// One straight run of a branch's **spine** (plan §13, D-cone-4): from the
/// bead `d0` steps out of the parent, `n` beads / `dx` columns long.
/// `hypot()` and `atan2()` give the bar its length and angle *in the
/// branch's own plane*, so the scene bends nowhere and the layout writes only
/// the slope (`--q`) — one number for the whole cone.
fn spine_bar(d0: usize, n: usize, dx: u64, live: bool) -> AnyView {
    view! {
        <div
            class=format!(
                "rw-br-line {}",
                if live { "active" } else { "abandoned" },
            )
            data-from=d0.to_string()
            data-to=(d0 + n).to_string()
            style=format!("--d0:{d0}; --n:{n}; --dx:{dx}")
        ></div>
    }
    .into_any()
}

/// The cone renderer (plan §13). One method, recursing down the branch
/// *forest*, so a fork off a branch knows its parent's bead and every branch
/// keeps one straight ray leaving it.
struct Cone<'a> {
    state: AppState,
    at: &'a HashMap<u64, (u64, i32, bool)>,
    by_seq: &'a HashMap<u64, FlowNode>,
    by_fin: &'a HashMap<u32, &'a FlowBranch>,
    parent_of: &'a HashMap<u64, u64>,
    kids: &'a HashMap<u32, Vec<u32>>,
    /// The server's ring constants, for the static part of a nested chain.
    step: i32,
    arc: i32,
}

/// Style B's flat projection shrinks a branch's outward step by this factor
/// (plan §13, D9/L10). It is the same law the stylesheet's `--kroot` uses, so
/// a nested bead can counter-scale by its chain's product and stay round.
fn flat_k(deg: f64) -> f64 {
    let r = deg.to_radians();
    r.cos() - 0.12 * r.sin()
}

impl Cone<'_> {
    /// One branch: a container sitting at (or inside) its parent's bead, one
    /// straight spine, its rounds, and the branches that fork off it.
    ///
    /// `outer` is the hinge of the container this one lives in (0 when it
    /// lives on the trunk) and `po` its parent bead's step inside that
    /// container; `slot` is the angle it takes. `sroot` is the slot of the
    /// topmost trunk-parented ancestor and `sum` the static part of the
    /// nesting chain — together they give the plane's *absolute* angle, which
    /// a bead needs to counter-rotate (the CSS cannot add an ancestor's own
    /// variable to its own without a cycle).
    #[allow(clippy::too_many_arguments)]
    fn branch(
        &self,
        i: u32,
        outer: u64,
        po: u32,
        slot: i32,
        sroot: i32,
        sum: i32,
        kup: f64,
        depth: u32,
    ) -> AnyView {
        let Some(b) = self.by_fin.get(&i).copied() else {
            return view! { <div></div> }.into_any();
        };
        // this container's own static flat factor (1 at the root: the root's
        // turn carries the phase, so the stylesheet reads that one from
        // `--root_a`). The children inherit it through `--kup`.
        let kown = if depth == 0 {
            1.0
        } else {
            flat_k((slot * self.step).clamp(-self.arc, self.arc) as f64)
        };
        let hinge = b.hinge_x;
        let seqs = &b.seqs;
        let n = seqs.len();
        let last_x = seqs
            .last()
            .and_then(|s| self.at.get(s))
            .map(|t| t.0)
            .unwrap_or(hinge);
        let dx = last_x.saturating_sub(hinge).max(1);
        // How far the live path runs into this branch: the spine is one
        // straight bar per run, so a branch that is lit where it leaves its
        // parent but buried further along still shows its dead tail as dead.
        let live = seqs
            .iter()
            .take_while(|s| self.at.get(s).map(|t| t.2).unwrap_or(false))
            .count();
        let spines: Vec<AnyView> = if live == 0 || live >= n {
            vec![spine_bar(0, n, dx, live > 0)]
        } else {
            vec![
                spine_bar(0, live, dx.min(live as u64).max(1), true),
                spine_bar(live, n - live, dx.saturating_sub(live as u64).max(1), false),
            ]
        };
        // the beads: the k-th round of the branch rides `k+1` steps out, so
        // the first one leaves the parent rather than sitting on it
        let beads: Vec<AnyView> = seqs
            .iter()
            .enumerate()
            .filter_map(|(k, s)| self.by_seq.get(s).map(|nd| (k as u32 + 1, nd.clone())))
            .map(|(out, nd)| flow_node_button(self.state, nd, out))
            .collect();
        // the branches that fork off this one, as nested containers
        let child_views: Vec<AnyView> = self
            .kids
            .get(&i)
            .map(|cs| {
                cs.iter()
                    .enumerate()
                    .map(|(k, &c)| {
                        // the child's parent round is *this* branch's bead at
                        // that column — where the child's spine must start
                        let p = self
                            .by_fin
                            .get(&c)
                            .and_then(|cb| self.parent_of.get(&cb.root))
                            .copied()
                            .unwrap_or(b.root);
                        let po = seqs.iter().position(|s| *s == p).map(|q| q as u32 + 1);
                        // D-cone-7: a nested fork fans *relative to its
                        // parent* — never a zero offset (which would lay the
                        // child's ray on top of the parent's next bead) and
                        // never the ring's absolute slots. The fan is **two**
                        // slots, not one: measured on the live `rewind`
                        // session, a +30° child put its first bead 4.5px from
                        // the parent's own continuation bead and *behind* it
                        // (z −16px), so the current round hid under a dead
                        // one. ±60° separates them by ~17px and puts the first
                        // child in front of the viewer.
                        let slot = if k % 2 == 0 {
                            -((k as i32) / 2 + 1) * 2
                        } else {
                            ((k as i32 + 1) / 2) * 2
                        };
                        let inner = (slot * self.step).clamp(-self.arc, self.arc);
                        // the child's `--kup` is every *ancestor* level's
                        // factor — this container's own included, the child's
                        // own excluded (the child adds that itself)
                        self.branch(
                            c,
                            hinge,
                            po.unwrap_or(0_u32),
                            slot,
                            sroot,
                            sum + inner,
                            kup * kown,
                            depth + 1,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // the container's own static flat factor (1 at the root: its turn
        // carries the phase, so the stylesheet reads it from `--root_a`)
        view! {
            <div
                class="rw-branch"
                data-fin=i.to_string()
                data-root=b.root.to_string()
                data-hinge=hinge.to_string()
                data-n=n.to_string()
                data-lane=b.lane.to_string()
                style=format!(
                    "--slot:{slot}; --hinge:{hinge}; --ph:{outer}; --po:{po}; --n:{n}; \
                     --dx:{dx}; --sroot:{sroot}; --sum:{sum}; \
                     --kup:{kup:.5}; --kown:{kown:.5}",
                )
            >
                { spines } { beads } { child_views }
            </div>
        }
        .into_any()
    }
}

/// One round: a dot on the line (or on its branch). A click only **selects**
/// it — the Rewind button in the panel is the only trigger in this style.
fn flow_node_button(state: AppState, n: FlowNode, out: u32) -> AnyView {
    let seq = n.seq;
    let FlowNode {
        x,
        lane,
        main,
        round,
        current,
        retracted,
        summary,
        ..
    } = n;
    let abandoned = n.state != "active";
    let blocked = n.restore.blocked();
    let selected = state.rw_selected;
    let title = format!(
        "round {round} \u{00b7} {summary}{}",
        if current {
            " \u{2014} you are here"
        } else if abandoned {
            " \u{2014} abandoned"
        } else {
            ""
        }
    );
    let cls = move || {
        let mut c = String::from("rw3-node");
        c.push_str(if main { " main" } else { " off" });
        if abandoned {
            c.push_str(" abandoned");
        }
        if current {
            c.push_str(" current");
        }
        if retracted {
            c.push_str(" retracted");
        }
        if blocked {
            c.push_str(" blocked");
        }
        if selected.get() == Some(seq) {
            c.push_str(" selected");
        }
        c
    };
    view! {
        <button
            class=cls
            data-seq=seq.to_string()
            data-x=x.to_string()
            data-lane=lane.to_string()
            data-main=main.to_string()
            data-current=current.to_string()
            data-out=out.to_string()
            title=title
            style=format!("--x:{x}; --lane:{lane}; --out:{out}")
            on:click=move |_| select_node(state, seq)
        >
            <span class="rw3-dot"></span>
            <span class="rw3-round">{ round.to_string() }</span>
        </button>
    }
    .into_any()
}

// ── the confirm dialog ────────────────────────────────────────────

/// Sidebar/card confirm step: "Rewind to this point?" → Cancel /
/// Rewind. On confirm, writes `mode:"on"` for the pending target and bumps
/// `rewind_gen` so the tree refetches even if the WS frame is missed.
pub fn rewind_confirm_dialog(state: AppState) -> AnyView {
    let open = state.rewind_pending;
    let err = RwSignal::new(Option::<String>::None);
    // The post-hoc notice: the marker landed but the kernel's
    // projection will drop it (the response verdict, D-C).
    let warn = RwSignal::new(Option::<String>::None);

    let close = move || {
        crate::ui::after_dispatch(move || {
            open.set(None);
            err.set(None);
            warn.set(None);
        });
    };

    // R3: what this node restores (the tree's annotation), and whether
    // the pick must be refused (the kernel would ignore the marker).
    let restore = move || {
        let seq = open.get().map(|t| t.seq)?;
        state.rewind_tree.get()?.restore_at(seq)
    };
    let blocked = move || restore().map(|r| r.blocked()).unwrap_or(false);
    let restore_note = move || match restore() {
        Some(r) if r.blocked() => match r {
            Restore::Unresumable { missing } => format!(
                "This point cannot be resumed: the context it restores would leave tool call `{missing}` without its result, so the loop would ignore the rewind. Pick the round's first message instead."
            ),
            _ => String::new(),
        },
        Some(r) => format!("The context resumes from {}.", r.label()),
        None => String::new(),
    };

    let confirm = move |_| {
        let Some(t) = open.get_untracked() else {
            return;
        };
        let Some(sess) = state.active_session.get_untracked() else {
            err.set(Some("no active session".to_string()));
            return;
        };
        if !rewind_allowed(state, &sess) {
            err.set(Some("the loop is running".to_string()));
            return;
        }
        // The pre-check in the tree: a pick the kernel would ignore is
        // refused before the write (plan 11.1/11.2).
        if blocked() {
            return;
        }
        warn.set(None);
        spawn_local(async move {
            match api::post_rewind(&sess, t.seq, "on").await {
                Ok(verdict) => {
                    state.rewind_gen.update(|g| *g += 1);
                    match verdict.notice() {
                        // The marker landed but the kernel's projection
                        // drops it: say so and stay open (D-C). The
                        // user can pick another round.
                        Some(n) => crate::ui::after_dispatch(move || {
                            warn.set(Some(n));
                            err.set(None);
                        }),
                        None => crate::ui::after_dispatch(move || {
                            open.set(None);
                            err.set(None);
                            warn.set(None);
                        }),
                    }
                }
                Err(e) => err.set(Some(e)),
            }
        });
    };

    view! {
        <Show when=move || open.get().is_some() fallback=|| ()>
            <div id="rw-backdrop" on:click=move |_| close()>
                <div id="rw-dialog" on:click=move |e: MouseEvent| e.stop_propagation()>
                    <div class="rw-title">{ "Rewind to this point?" }</div>
                    { move || open.get().map(|t| view! {
                        <div class="rw-body">
                            <div class="rw-target">{ t.label }</div>
                            <div class="rw-note">
                                { "The active conversation resumes from here. Everything after it moves to an abandoned branch \u{2014} it stays in the history tree, and you can rewind back to it later." }
                            </div>
                        </div>
                    }) }
                    { move || {
                        let note = restore_note();
                        (!note.is_empty()).then(|| view! {
                            <div class="rw-restore-note" class:rw-block=blocked()>{ note }</div>
                        })
                    } }
                    { move || err.get().map(|e| view! { <div class="rw-err">{ e }</div> }) }
                    { move || warn.get().map(|w| view! { <div class="rw-warn">{ w }</div> }) }
                    <div class="rw-actions">
                        <button class="ns-btn ns-cancel" on:click=move |_| close()>{ "Cancel" }</button>
                        <button
                            class="ns-btn ns-danger"
                            disabled=move || blocked() || match state.active_session.get() {
                                Some(s) => !rewind_allowed(state, &s),
                                None => true,
                            }
                            on:click=confirm
                        >
                            { "Rewind" }
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
    .into_any()
}

/// The transcript quick button (`\u{27f2}`) on a user-message card: opens the
/// same confirm dialog for `card_seq` (the card's own log line —
/// `hist_oldest_line + card index`, the same 1-based non-empty-line counting
/// the kernel's `seq` uses). Disabled while the loop runs and on the current
/// tail (there is nothing to abandon there).
pub fn quick_rewind_button(state: AppState, card_seq: u64, excerpt: String) -> AnyView {
    let at_tail = move || state.rewind_tree.get().and_then(|t| t.current_seq) == Some(card_seq);
    let locked = move || {
        state
            .active_session
            .get()
            .map(|s| !rewind_allowed(state, &s))
            .unwrap_or(true)
    };
    // R1/R3: the tree's annotation for this card's line, when the tree
    // has it (a card outside the tree — an edited log — stays pickable
    // and the dialog's post-hoc notice covers it).
    let restore = move || state.rewind_tree.get()?.restore_at(card_seq);
    let blocked = move || restore().map(|r| r.blocked()).unwrap_or(false);
    let title = move || {
        if locked() {
            "rewind is disabled while the loop is running".to_string()
        } else if at_tail() {
            "you are already here".to_string()
        } else if blocked() {
            "this point cannot be resumed: it would strand a tool call".to_string()
        } else {
            "rewind to this message".to_string()
        }
    };
    let on_click = move |ev: MouseEvent| {
        // The card itself is clickable (compact/unfold in the pile engine).
        ev.stop_propagation();
        let Some(s) = state.active_session.get_untracked() else {
            return;
        };
        if locked() || at_tail() || !rewind_allowed(state, &s) {
            return;
        }
        request_rewind(state, card_seq, excerpt.clone());
    };
    view! {
        <div class="ev-quick-row">
            <button
                class="ev-quickbtn"
                title=title
                disabled=move || locked() || at_tail()
                on:click=on_click
            >
                { "\u{27f2}" }
            </button>
        </div>
    }
    .into_any()
}

// ── the sidebar plugin panel (registered in plugins.rs) ───────────

/// Compact panel: what the plugin is, the active path, and the full-view
/// entry. Rules 3/6 of `docs/plugin-authoring-rules.md` (one scroller, pinned
/// footer, fetch on mount/session change through the shared tree effect).
pub fn rewind_plugin_view(state: AppState) -> AnyView {
    let tree = state.rewind_tree;
    let sess = state.active_session;
    let layout = state.layout_mode;

    view! {
        <div id="rewind-body">
            { move || match (sess.get(), tree.get()) {
                (None, _) => view! {
                    <div class="plugin-empty">{ "no session selected" }</div>
                }.into_any(),
                (_, None) => view! {
                    <div class="plugin-empty">{ "loading\u{2026}" }</div>
                }.into_any(),
                (_, Some(t)) if t.total_rounds == 0 => view! {
                    <div class="plugin-empty">{ "no rounds yet" }</div>
                }.into_any(),
                (_, Some(t)) => {
                    let path = t.active_path();
                    let n = t.total_rounds;
                    let a = t.active_rounds();
                    let ab = t.abandoned_rounds();
                    let notice = t.tail_ignored.as_ref().map(|m| m.notice());
                    view! {
                        { notice.map(|msg| view! {
                            <div class="rewind-notice">{ msg }</div>
                        }) }
                        <div class="rewind-meta">
                            { format!("{n} rounds \u{00b7} {a} active \u{00b7} {ab} abandoned") }
                        </div>
                        <div class="rewind-list">
                            <For
                                each=move || path.clone()
                                key=|x: &RewindNode| x.seq
                                children=move |x: RewindNode| active_row(state, x)
                            />
                        </div>
                    }.into_any()
                }
            } }
        </div>
        <div class="rewind-actions">
            <button
                id="rewind-open-hist"
                title="open the full history tree"
                on:click=move |_| {
                    crate::ui::set_layout_mode("full");
                    layout.set("full".to_string());
                }
            >
                { "open History view" }
            </button>
        </div>
    }
    .into_any()
}

/// One active-path row in the sidebar panel: click → the same confirm dialog.
fn active_row(state: AppState, n: RewindNode) -> AnyView {
    let seq = n.seq;
    let round = n.round;
    let summary = n.summary.clone();
    let current = n.current;
    let label = node_label(&n);
    let row_cls = move || {
        let locked = state
            .active_session
            .get()
            .map(|s| !rewind_allowed(state, &s))
            .unwrap_or(true);
        let mut c = String::from("rewind-row");
        if current {
            c.push_str(" current");
        }
        if locked {
            c.push_str(" locked");
        }
        c
    };
    let on_click = move |_| {
        let Some(s) = state.active_session.get_untracked() else {
            return;
        };
        if current || !rewind_allowed(state, &s) {
            return;
        }
        request_rewind(state, seq, label.clone());
    };
    view! {
        <button class=row_cls on:click=on_click>
            <span class="rewind-row-round">{ format!("r{round}") }</span>
            <span class="rewind-row-sum">{ summary }</span>
            <Show when=move || current fallback=|| ()>
                <span class="rewind-row-here">{ "here" }</span>
            </Show>
        </button>
    }
    .into_any()
}
