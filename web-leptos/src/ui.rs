//! Sidebar / goal / context bar / input / welcome (port of the JS DOM code).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use web_sys::{HtmlSelectElement, KeyboardEvent, MouseEvent};
use wasm_bindgen::JsCast;

use crate::api;
use crate::model::{compute_rounds, GoalView, AppState};
use crate::timeutil;
use crate::ws;

/// Context budget (tokens) shown by the context bar (port of ctxBudget).
pub const CTX_BUDGET: u64 = 262_144;

// ── sidebar collapse (persisted) ──────────────────────────────────
pub fn read_collapsed() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("rushi-sidebar-collapsed").ok())
        .flatten()
        .as_deref()
        == Some("1")
}

fn set_collapsed(collapsed: bool) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item("rushi-sidebar-collapsed", if collapsed { "1" } else { "0" });
    }
}

// ── session selection / mutation helpers ──────────────────────────
pub fn select_session(state: AppState, name: &str) {
    state.active_session.set(Some(name.to_string()));
    state.events.set(Vec::new());
    state.view_round.set(None);
    state.ctx_used.set(0);
    state.rounds_ctxk.set(Vec::new());
    state.goal.set(None);
    state.menu_session.set(None);

    let s2 = state;
    let name2 = name.to_string();
    spawn_local(async move {
        if let Ok(sessions) = api::load_sessions().await {
            s2.sessions.set(sessions);
        }
        ws::connect(&s2, &name2);
        s2.goal.set(api::load_goal(&name2).await);
        s2.loop_running.set(api::loop_running(&name2).await);
    });
}

pub fn delete_session(state: AppState, name: &str) {
    let active = state.active_session.get();
    let _ = api::delete_session(name);
    if active.as_deref() == Some(name) {
        state.active_session.set(None);
        state.events.set(Vec::new());
        state.view_round.set(None);
        state.ctx_used.set(0);
        state.rounds_ctxk.set(Vec::new());
        state.goal.set(None);
        ws::close_current();
        state.ws_status.set("disconnected".to_string());
    }
    let s2 = state;
    spawn_local(async move {
        if let Ok(sessions) = api::load_sessions().await {
            s2.sessions.set(sessions);
        }
    });
}

pub fn rename_session(state: AppState, old: &str) {
    let Some(new_name) = web_sys::window()
        .and_then(|w| w.prompt_with_message_and_default("Rename session:", old).ok())
        .flatten()
    else {
        return;
    };
    if new_name.is_empty() || new_name == old {
        return;
    }
    let state2 = state;
    let old_owned = old.to_string();
    let new_owned = new_name.to_string();
    spawn_local(async move {
        if let Err(e) = api::rename_session(&old_owned, &new_owned).await {
            if let Some(w) = web_sys::window() {
                let _ = w.alert_with_message(&format!("rename failed: {e}"));
            }
            return;
        }
        if state2.active_session.get().as_deref() == Some(old_owned.as_str()) {
            select_session(state2, &new_owned);
        }
    });
}

pub fn new_session(state: AppState) {
    // Open the new-session dialog, prefilled with the server's default
    // working directory.
    spawn_local(async move {
        let def = api::default_cwd().await.unwrap_or_default();
        state.new_session_open.set(Some(def));
    });
}

pub fn start_loop(state: AppState) {
    let Some(id) = state.active_session.get() else {
        if let Some(w) = web_sys::window() {
            let _ = w.alert_with_message("Select or create a session first");
        }
        return;
    };
    if ws::is_open() {
        ws::send_command(&id, &json!({ "kind": "start" }));
    } else {
        let s = state;
        spawn_local(async move {
            let _ = api::start_loop(&id).await;
            s.loop_running.set(true);
        });
    }
    state.loop_running.set(true);
}

pub fn stop_loop(state: AppState) {
    let Some(id) = state.active_session.get() else { return };
    if ws::is_open() {
        ws::send_command(&id, &json!({ "kind": "stop" }));
    } else {
        let _ = api::stop_loop(&id);
    }
    state.loop_running.set(false);
}

pub async fn do_send(state: AppState, content: String, queue: String) {
    let content = content.trim().to_string();
    if content.is_empty() {
        return;
    }
    let Some(active) = state.active_session.get() else {
        if let Some(w) = web_sys::window() {
            let _ = w.alert_with_message("Select or create a session first");
        }
        return;
    };
    let queue_opt = if queue.is_empty() { None } else { Some(queue) };

    // optimistic local render (port of the doSend local push).
    // The `optimistic` flag lets the WS echo path replace this card
    // with the canonical server event instead of rendering it twice.
    let mut ev = json!({
        "type": "user_message",
        "ts": timeutil::now_iso(),
        "content": content,
        "optimistic": true,
    });
    if let Some(q) = &queue_opt {
        ev["queue"] = json!(q);
    }
    // The local push closes the in-flight round (legacy curRound
    // bookkeeping): record the ctxK at round close, then append.
    if !state.events.with(|v| v.is_empty()) {
        state.rounds_ctxk.update(|v| v.push(state.ctx_used.get()));
    }
    state.events.update(|v| v.push(ev.clone()));

    if ws::is_open() {
        let mut payload = json!({ "kind": "message", "content": content });
        if let Some(q) = &queue_opt {
            payload["queue"] = json!(q);
        }
        ws::send_command(&active, &payload);
    } else {
        let active2 = active.clone();
        let content2 = content.clone();
        spawn_local(async move {
            if let Err(e) = api::post_message(&active2, &content2, queue_opt.as_deref()).await {
                if let Some(w) = web_sys::window() {
                    let _ = w.alert_with_message(&format!("send failed: {e}"));
                }
            }
        });
    }

    // ensure the loop is running (port of ensureLoopRunning)
    let state3 = state;
    let active3 = active.clone();
    spawn_local(async move {
        if !api::loop_running(&active3).await {
            if ws::is_open() {
                ws::send_command(&active3, &json!({ "kind": "start" }));
            } else {
                let _ = api::start_loop(&active3).await;
            }
            state3.loop_running.set(true);
        }
    });
}

// ── sidebar ─────────────────────────────────────────────────────────
#[component]
pub fn Sidebar(state: AppState) -> impl IntoView {
    let sessions = state.sessions;
    let active = state.active_session;
    let loop_running = state.loop_running;
    let ws_status = state.ws_status;
    let collapsed = state.sidebar_collapsed;

    // The menu state lives in AppState so the ported document-level
    // click/Escape handlers (pile::init) can close it.
    let menu_session = state.menu_session;
    let menu_pos = state.menu_pos;

    let ws_dot_class = move || {
        let s = ws_status.get();
        match s.as_str() {
            "connected" => "ws-dot connected".to_string(),
            "connecting" => "ws-dot connecting".to_string(),
            "error" => "ws-dot error".to_string(),
            _ => "ws-dot disconnected".to_string(),
        }
    };

    let sidebar_cls = move || {
        if collapsed.get() { "collapsed".to_string() } else { String::new() }
    };

    let start_cls = move || {
        if loop_running.get() { "running".to_string() } else { String::new() }
    };

    view! {
        <aside id="sidebar" class=sidebar_cls>
            <div id="sidebar-inner">
                <div class="sb-header">
                    <h1>{ "rushi web" }</h1>
                    <button
                        id="sidebar-toggle"
                        title="collapse sidebar"
                        on:click=move |_| {
                            let v = !collapsed.get();
                            collapsed.set(v);
                            set_collapsed(v);
                        }
                    >
                        { "\u{2039}" }
                    </button>
                </div>
                <div id="session-list">
                    <For
                        each=move || sessions.get()
                        key=|s: &crate::model::SessionInfo| s.name.clone()
                        children=move |s| {
                            let s_name = s.name.clone();
                            let s_ts = s.last_modified;
                            let click_name = s_name.clone();
                            let menu_name = s_name.clone();
                            let item_cls_name = s_name.clone();
                            let item_cls = move || {
                                let mut c = String::from("session-item");
                                if active.get().as_deref() == Some(item_cls_name.as_str()) {
                                    c.push_str(" active");
                                    if loop_running.get() {
                                        c.push_str(" running");
                                    }
                                }
                                c
                            };
                            view! {
                                <div
                                    class=item_cls
                                    on:click=move |_| {
                                        select_session(state, &click_name);
                                        menu_session.set(None);
                                    }
                                >
                                    <span class="sname">{ s_name }</span>
                                    <span class="smeta">
                                        { move || {
                                            s_ts.map(|ts| {
                                                let ms = (ts * 1000.0) as f64;
                                                let d =
                                                    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms));
                                                let h = d.get_hours();
                                                let mi = d.get_minutes();
                                                let sec = d.get_seconds();
                                                format!("{h:02}:{mi:02}:{sec:02}")
                                            }).unwrap_or_else(|| "no events".to_string())
                                        } }
                                    </span>
                                    <button
                                        class="sess-more"
                                        title="session actions"
                                        on:click=move |e: MouseEvent| {
                                            e.stop_propagation();
                                            menu_pos.set((
                                                e.client_x() as f64,
                                                e.client_y() as f64,
                                            ));
                                            menu_session.set(Some(menu_name.clone()));
                                        }
                                    >
                                        { "\u{2026}" }
                                    </button>
                                </div>
                            }
                        }
                    />
                </div>
                <GoalPanel state=state />
                <div id="session-actions">
                    <button
                        id="btn-new"
                        on:click=move |_| { new_session(state); }
                    >
                        { "+ New Session" }
                    </button>
                    <button
                        id="btn-start"
                        class=start_cls
                        on:click=move |_| { start_loop(state); }
                    >
                        { "\u{25B6} Start Loop" }
                    </button>
                    <button
                        id="btn-stop"
                        on:click=move |_| { stop_loop(state); }
                    >
                        { "\u{25A0} Stop Loop" }
                    </button>
                </div>
                <div id="status-bar">
                    <span class=ws_dot_class>{ move || ws_status.get() }</span>
                </div>
            </div>
        </aside>
        // session "..." menu (rename / delete)
        <Show
            when=move || menu_session.get().is_some()
            fallback=|| ()
        >
            { move || {
                let pos = menu_pos.get();
                let style = format!("left:{:.0}px; top:{:.0}px; display:block;", pos.0, pos.1);
                view! {
                    <div
                        id="sess-menu"
                        style=style
                        on:click=move |e: web_sys::MouseEvent| { e.stop_propagation(); }
                    >
                        <button
                            class="sess-menu-item"
                            on:click=move |_| {
                                let name = menu_session.get().unwrap_or_default();
                                menu_session.set(None);
                                if !name.is_empty() {
                                    rename_session(state, &name);
                                }
                            }
                        >
                            { "rename" }
                        </button>
                        <button
                            class="sess-menu-item danger"
                            on:click=move |_| {
                                let name = menu_session.get().unwrap_or_default();
                                menu_session.set(None);
                                if !name.is_empty()
                                    && web_sys::window()
                                        .and_then(|w| w.confirm_with_message(&format!(
                                            "Delete session \"{}\" and all its data?",
                                            name
                                        )).ok())
                                        .unwrap_or(false)
                                {
                                    delete_session(state, &name);
                                }
                            }
                        >
                            { "delete" }
                        </button>
                    </div>
                }
            } }
        </Show>
    }
}

// ── goal panel ─────────────────────────────────────────────────────
#[component]
fn GoalPanel(state: AppState) -> impl IntoView {
    let goal = state.goal;

    view! {
        <div id="goal-panel">
            <div class="goal-header">{ "goal" }</div>
            <div id="goal-body">
                { move || match goal.get() {
                    Some(ref g) => goal_body_view(g),
                    None => view! { <div class="goal-empty">{ "no goal" }</div> }.into_any(),
                } }
            </div>
            <div class="goal-actions">
                <button
                    id="goal-create"
                    on:click=move |_| {
                        let Some(text) = web_sys::window()
                            .and_then(|w| w.prompt_with_message("New goal:").ok())
                            .flatten()
                        else {
                            return;
                        };
                        let t = text.trim().to_string();
                        if t.is_empty() {
                            return;
                        }
                        let s = state;
                        spawn_local(async move {
                            let id = s.active_session.get().unwrap_or_default();
                            api::goal_action(&id, "create", Some(&t)).await;
                            s.goal.set(api::load_goal(&id).await);
                        });
                    }
                >
                    { "new" }
                </button>
                <button
                    id="goal-pause"
                    on:click=move |_| {
                        let s = state;
                        spawn_local(async move {
                            let id = s.active_session.get().unwrap_or_default();
                            api::goal_action(&id, "pause", None).await;
                            s.goal.set(api::load_goal(&id).await);
                        });
                    }
                >
                    { "pause" }
                </button>
                <button
                    id="goal-resume"
                    on:click=move |_| {
                        let s = state;
                        spawn_local(async move {
                            let id = s.active_session.get().unwrap_or_default();
                            api::goal_action(&id, "resume", None).await;
                            s.goal.set(api::load_goal(&id).await);
                        });
                    }
                >
                    { "resume" }
                </button>
                <button
                    id="goal-clear"
                    on:click=move |_| {
                        if web_sys::window()
                            .and_then(|w| w.confirm_with_message("Clear the current goal?").ok())
                            .unwrap_or(false)
                        {
                            let s = state;
                            spawn_local(async move {
                                let id = s.active_session.get().unwrap_or_default();
                                api::goal_action(&id, "clear", None).await;
                                s.goal.set(api::load_goal(&id).await);
                            });
                        }
                    }
                >
                    { "clear" }
                </button>
            </div>
        </div>
    }
}

fn goal_body_view(g: &GoalView) -> AnyView {
    let status = g.status().to_string();
    let meta_base = format!(
        "iter {} \u{b7} {} tok \u{b7} {}",
        g.iteration,
        g.used_tokens,
        g.id.as_deref().unwrap_or("?")
    );
    let meta = match g.block_reason {
        Some(ref r) if !r.is_empty() => format!("{meta_base}\n{r}"),
        _ => meta_base,
    };
    let text = g.goal.clone().unwrap_or_default();
    let badge_cls = format!("goal-badge {status}");
    view! {
        <span class=badge_cls>{ status }</span>
        <div class="goal-text">{ text }</div>
        <div class="goal-meta">{ meta }</div>
    }.into_any()
}

// ── context bar (port of updateCtxBar / renderRounds / setView) ──
#[component]
pub fn ContextBar(state: AppState) -> impl IntoView {
    let ctx_used = state.ctx_used;
    let view_round = state.view_round;
    let events = state.events;
    let collapsed = state.sidebar_collapsed;

    let fill_style = move || {
        let used = ctx_used.get();
        let pct = (used as f64 / CTX_BUDGET as f64 * 100.0).min(100.0);
        let color = if pct > 90.0 {
            "var(--danger)"
        } else if pct > 70.0 {
            "var(--warn)"
        } else {
            "var(--accent)"
        };
        format!("width:{pct:.1}%; background:{color};")
    };

    let ctx_pct = move || {
        let pct = (ctx_used.get() as f64 / CTX_BUDGET as f64 * 100.0).min(100.0);
        format!("{pct:.1}%")
    };

    let ctx_k = move || {
        format!(
            "~{}K / {}K",
            (ctx_used.get() / 1000),
            (CTX_BUDGET / 1000)
        )
    };

    view! {
        <div id="context-bar">
            <div id="ctx-row">
                <Show when=move || collapsed.get() fallback=|| ()>
                    <button
                        id="sidebar-open"
                        title="expand sidebar"
                        on:click=move |_| {
                            collapsed.set(false);
                            set_collapsed(false);
                        }
                    >
                        { "\u{00bb}" }
                    </button>
                </Show>
                <span id="ctx-label">{ "context" }</span>
                <div id="ctx-track">
                    <div id="ctx-fill" style=fill_style />
                </div>
                <span id="ctx-pct">{ ctx_pct }</span>
                <span id="ctx-k">{ ctx_k }</span>
            </div>
            <div id="ctx-rounds">
                { move || rounds_view(events, view_round, state) }
            </div>
        </div>
    }
}

/// renderRounds(): max(6, n) slots; slot i<n = round chip, otherwise
/// a hidden placeholder. Legacy parity: chips are empty 26x10 buttons
/// with all info in the `title` attribute.
fn rounds_view(
    events: RwSignal<Vec<serde_json::Value>>,
    view_round: RwSignal<Option<usize>>,
    state: AppState,
) -> AnyView {
    let rounds = compute_rounds(&events.get());
    let n = rounds.len();
    let slots = 6.max(n);
    let evs = events.get();
    let ctxk = state.rounds_ctxk.get();

    let chips: Vec<AnyView> = (0..slots).map(|i| {
        if i < n {
            let range = &rounds[i];
            let user_text = evs
                .get(range.start)
                .and_then(|e| e.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let k = ctxk.get(i).copied().unwrap_or(0);
            let mut lines: Vec<String> = Vec::new();
            if !user_text.is_empty() {
                lines.push(format!(
                    "\u{201C}{}\u{201D}",
                    crate::markdown::brief_text(&user_text, 40)
                ));
            }
            let k_part = if k > 0 {
                format!(" \u{b7} ~{}K", ((k as f64) / 1000.0).round() as u64)
            } else {
                String::new()
            };
            lines.push(format!("round {}{k_part}", i + 1));
            let title = lines.join("\n");
            let i2 = i;
            let state2 = state;
            let vr = view_round;
            let chip_cls = move || {
                let mut c = String::from("ctx-round");
                if vr.get() == Some(i2) {
                    c.push_str(" on");
                }
                c
            };
            view! {
                <button
                    class=chip_cls
                    title=title.clone()
                    on:click=move |_| {
                        state2.view_round.set(Some(i2));
                    }
                />
            }.into_any()
        } else {
            view! { <button class="ctx-round ctx-spot" disabled=true /> }.into_any()
        }
    }).collect();

    view! { <div>{ chips }</div> }.into_any()
}

// ── status strip (port of updateStatusStrip) ──────────────────────
#[component]
pub fn StatusStrip(state: AppState) -> impl IntoView {
    let events = state.events;

    view! {
        <div id="status-strip">
            { move || status_strip_chips(&events.get()) }
        </div>
    }
}

fn status_strip_chips(events: &[serde_json::Value]) -> AnyView {
    use std::collections::HashMap;
    let mut latest: HashMap<String, String> = HashMap::new();
    for ev in events {
        if ev.get("type").and_then(|v| v.as_str()) != Some("ext_status") {
            continue;
        }
        let Some(id) = ev.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let v = ev.get("value").cloned();
        let s = match &v {
            Some(serde_json::Value::Null) => "null".to_string(),
            Some(x @ serde_json::Value::Object(_) | x @ serde_json::Value::Array(_)) => x.to_string(),
            Some(x) => x.to_string(),
            None => String::new(),
        };
        latest.insert(id.to_string(), s);
    }
    let order = ["loop_phase", "model_thinking", "hook_applied", "model_call_context"];
    let mut ids: Vec<String> = order
        .iter()
        .filter_map(|id| latest.get(*id).cloned())
        .collect();
    let mut extras: Vec<String> = latest
        .keys()
        .filter(|k| !order.iter().any(|o| *o == k.as_str()))
        .cloned()
        .collect();
    extras.sort();
    ids.extend(extras);

    let chips: Vec<AnyView> = ids.iter().map(|id| {
        let val = latest.get(id).cloned().unwrap_or_default();
        view! {
            <span class="chip">
                <span class="lbl">{ format!("{id}:") }</span> <b>{ val }</b>
            </span>
        }.into_any()
    }).collect();

    view! { <div>{ chips }</div> }.into_any()
}

// ── new-session dialog (name + working-directory picker) ──────────
/// A modal that lets the user name a session AND choose the working
/// directory its agent loop runs in. Browsers cannot return a real
/// absolute path from a native directory input, so a small server-driven
/// directory browser (`/api/browse`) backs the picker.
#[component]
pub fn NewSessionDialog(state: AppState) -> impl IntoView {
    let open = state.new_session_open;
    let name = RwSignal::new(String::new());
    let cwd = RwSignal::new(String::new());
    let dirs = RwSignal::new(Vec::<String>::new());
    let parent = RwSignal::new(Option::<String>::None);
    let browse_err = RwSignal::new(Option::<String>::None);
    let create_err = RwSignal::new(Option::<String>::None);
    let busy = RwSignal::new(false);

    // Browse into a directory: updates the cwd field + the dir list.
    let browse_to = move |path: Option<String>| {
        dirs.set(Vec::new());
        browse_err.set(None);
        spawn_local(async move {
            match api::browse(path.as_deref()).await {
                Ok(b) => {
                    cwd.set(b.path.clone());
                    parent.set(b.parent);
                    dirs.set(b.dirs);
                    browse_err.set(b.error);
                }
                Err(e) => browse_err.set(Some(e)),
            }
        });
    };

    // (Re)initialize whenever the dialog opens.
    Effect::new(move || {
        if let Some(def) = open.get() {
            name.set(String::new());
            create_err.set(None);
            let def = if def.is_empty() { None } else { Some(def) };
            browse_to(def);
        }
    });

    let close = move || open.set(None);

    let create = move || {
        let n = name.get().trim().to_string();
        if n.is_empty() {
            create_err.set(Some("name is empty".to_string()));
            return;
        }
        let c = cwd.get();
        let cwd_opt = if c.trim().is_empty() { None } else { Some(c.trim().to_string()) };
        busy.set(true);
        create_err.set(None);
        spawn_local(async move {
            match api::create_session(&n, cwd_opt.as_deref()).await {
                Ok(()) => {
                    open.set(None);
                    select_session(state, &n);
                }
                Err(e) => create_err.set(Some(e)),
            }
            busy.set(false);
        });
    };

    view! {
        <Show when=move || open.get().is_some() fallback=|| ()>
            <div id="ns-backdrop" on:click=move |_| close()>
                <div id="ns-dialog" on:click=move |e: MouseEvent| e.stop_propagation()>
                    <div class="ns-title">{ "New Session" }</div>

                    <label class="ns-label" for="ns-name">{ "名称 Name" }</label>
                    <input id="ns-name" class="ns-input" placeholder="session name" bind:value=name />

                    <label class="ns-label" for="ns-cwd">{ "工作目录 Working directory" }</label>
                    <input id="ns-cwd" class="ns-input" placeholder="/path/to/project" bind:value=cwd />

                    <div class="ns-browser">
                        <div class="ns-dir ns-up" on:click=move |_| browse_to(parent.get())>
                            { "\u{2191} .." }
                        </div>
                        <For
                            each=move || dirs.get()
                            key=|d| d.clone()
                            children=move |d| {
                                let d2 = d.clone();
                                view! {
                                    <div class="ns-dir" on:click=move |_| {
                                        let base = cwd.get();
                                        let base = base.trim_end_matches('/');
                                        browse_to(Some(format!("{base}/{d2}")));
                                    }>
                                        { d }
                                    </div>
                                }
                            }
                        />
                        { move || {
                            if dirs.get().is_empty() && browse_err.get().is_none() {
                                Some(view! { <div class="ns-empty">{ "(no subdirectories)" }</div> })
                            } else {
                                None
                            }
                        } }
                    </div>

                    { move || browse_err.get().map(|e| view! { <div class="ns-err">{ e }</div> }) }
                    { move || create_err.get().map(|e| view! { <div class="ns-err">{ e }</div> }) }

                    <div class="ns-actions">
                        <button class="ns-btn ns-cancel" on:click=move |_| close()>{ "Cancel" }</button>
                        <button
                            class="ns-btn ns-create"
                            disabled=move || busy.get()
                            on:click=move |_| create()
                        >
                            { move || if busy.get() { "Creating…" } else { "Create" } }
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

// ── input module (port of doSend / keydown) ───────────────────────
#[component]
pub fn InputModule(state: AppState) -> impl IntoView {
    let msg = RwSignal::new(String::new());

    view! {
        <div id="input-module">
            <StatusStrip state=state />
            <div id="input-area">
                <select id="queue-select">
                    <option value="">direct</option>
                    <option value="steer">steer</option>
                    <option value="follow">follow</option>
                </select>
                <textarea
                    id="msg-input"
                    placeholder="Send a message... (Ctrl+Enter to send)"
                    rows="1"
                    bind:value=msg
                    on:keydown=move |e: KeyboardEvent| {
                        if e.key() == "Enter" && (e.ctrl_key() || e.meta_key()) {
                            e.prevent_default();
                            let q = web_sys::window()
                                .and_then(|w| w.document())
                                .and_then(|d| d.get_element_by_id("queue-select"))
                                .and_then(|el| Some(el.unchecked_into::<HtmlSelectElement>().value()))
                                .unwrap_or_default();
                            let text = msg.get();
                            msg.set(String::new());
                            let s = state;
                            spawn_local(async move {
                                do_send(s, text, q).await;
                            });
                        }
                    }
                />
                <button
                    id="btn-send"
                    on:click=move |_| {
                        let q = web_sys::window()
                            .and_then(|w| w.document())
                            .and_then(|d| d.get_element_by_id("queue-select"))
                            .and_then(|el| Some(el.unchecked_into::<HtmlSelectElement>().value()))
                            .unwrap_or_default();
                        let text = msg.get();
                        msg.set(String::new());
                        let s = state;
                        spawn_local(async move {
                            do_send(s, text, q).await;
                        });
                    }
                >
                    { "Send" }
                </button>
            </div>
        </div>
    }
}

// ── welcome (port of the static #welcome block) ───────────────────
#[component]
pub fn Welcome(state: AppState) -> impl IntoView {
    view! {
        <div id="welcome">
            <div class="w-hero">
                <div class="w-dish" aria_hidden="true">
                    <div class="w-plate" />
                    <div class="w-nigiri">
                        <div class="w-fish" />
                        <div class="w-rice" />
                        <div class="w-nori" />
                    </div>
                    <div class="w-wasabi" />
                    <div class="w-maki" />
                </div>
                <h1 class="w-title">
                    { "Welcome to " }
                    <span>{ "Rushi" }</span>
                </h1>
                <div class="w-chip">{ "\u{2699} rust \u{00d7} sushi \u{2192} rushi" }</div>
                <p class="w-sub">
                    { "a single-file harness for agent sessions \u{2014} events rolled up and served, omakase-style" }
                </p>
                <button id="w-new" on:click=move |_| { new_session(state); }>
                    { "Roll a new session" }
                </button>
                <div class="w-hint">{ "or pick one from the sidebar" }</div>
            </div>
        </div>
    }
}
