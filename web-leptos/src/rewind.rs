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

use leptos::prelude::*;
use leptos::task::spawn_local;
use web_sys::MouseEvent;

use crate::api;
use crate::model::{AppState, RewindNode, RewindTarget, SessionInfo};
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
                Err(_) => tree.set(None),
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
                <div id="hist-tree">{ hist_tree(state) }</div>
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
                        { nodes.into_iter().map(|n| node_view(state, sess.clone(), n)).collect_view() }
                    </div>
                    { boundaries_view }
                }.into_any()
            }
        } }
    }
    .into_any()
}

/// One round node + its children (recursive). Abandoned branches keep their
/// dimmed styling; the current one carries the "here" marker.
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
        c
    };
    let sess_title = sess.clone();
    let ts_title = ts.clone();
    let state_title = node.state.clone();
    let title = move || {
        let head = format!(
            "round {round} \u{00b7} {ts_title} \u{00b7} {events} event{} \u{00b7} {state_title}",
            if events == 1 { "" } else { "s" }
        );
        if !rewind_allowed(state, &sess_title) {
            format!("{head} \u{2014} rewind is disabled while the loop is running")
        } else if current {
            format!("{head} \u{2014} you are here")
        } else {
            format!("{head} \u{2014} click to rewind to this point")
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

    view! {
        <div class=node_cls data-seq=seq.to_string() data-round=round.to_string()>
            <div class="rw-head" title=title on:click=on_click>
                <span class="rw-dot"></span>
                <span class="rw-round">{ format!("round {round}") }</span>
                <span class="rw-sum">{ summary.clone() }</span>
                { badge }
                <span class="rw-meta">
                    { format!("{events} event{}", if events == 1 { "" } else { "s" }) }
                </span>
                <span class="rw-time">{ ts.clone() }</span>
                <Show when=move || current fallback=|| ()>
                    <span class="rw-here">{ "here" }</span>
                </Show>
            </div>
            <div class="rw-kids">
                { kids.into_iter().map(|k| node_view(state, sess.clone(), k)).collect_view() }
            </div>
        </div>
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

    let close = move || {
        crate::ui::after_dispatch(move || {
            open.set(None);
            err.set(None);
        });
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
        spawn_local(async move {
            match api::post_rewind(&sess, t.seq, "on").await {
                Ok(()) => {
                    state.rewind_gen.update(|g| *g += 1);
                    crate::ui::after_dispatch(move || {
                        open.set(None);
                        err.set(None);
                    });
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
                    { move || err.get().map(|e| view! { <div class="rw-err">{ e }</div> }) }
                    <div class="rw-actions">
                        <button class="ns-btn ns-cancel" on:click=move |_| close()>{ "Cancel" }</button>
                        <button
                            class="ns-btn ns-danger"
                            disabled=move || match state.active_session.get() {
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
    let title = move || {
        if locked() {
            "rewind is disabled while the loop is running".to_string()
        } else if at_tail() {
            "you are already here".to_string()
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
                    view! {
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
