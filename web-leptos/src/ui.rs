//! Sidebar / goal / context bar / input / welcome (port of the JS DOM code).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use web_sys::{DragEvent, KeyboardEvent, MouseEvent};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::api;
use crate::model::{
    AppState, GoalView, SessionInfo, DirEntry, compute_rounds,
    dispatch_group_label, dispatch_groups, ordered_sessions,
    RpTab, RpTabKind,
};
use crate::timeutil;
use crate::ws;
use crate::WEBUI_VERSION;

/// Run `f` in a macrotask (setTimeout 0), i.e. after the current event
/// dispatch has fully finished. Needed for closing menus / dialogs: this
/// Chromium build drains microtasks *mid* event dispatch (between the
/// target handler and the bubble listeners), so a `spawn_local`-deferred
/// unmount can land mid-dispatch — the detached element's listeners then
/// fire on freed wasm closures and throw "closure invoked after being
/// dropped". A macrotask runs only after the dispatch is done.
pub(crate) fn after_dispatch(f: impl FnOnce() + 'static) {
    if let Some(w) = web_sys::window() {
        let cb = Closure::once(f);
        let f: &js_sys::Function = cb.as_js_value().unchecked_ref();
        let _ = w.set_timeout_with_callback(f);
        // Transfer ownership to the timer. Without forget(), the Rust
        // handle's drop would unref the JS side and the callback could
        // fire on freed state ("closure invoked after being dropped").
        cb.forget();
    } else {
        f();
    }
}

/// v0.5.46: does this click come from inside `sel` (that element or a
/// descendant)? Cards use it to ignore clicks that belong to their own
/// popups: a popup that closes inside its click handler is unmounted
/// mid-dispatch and can no longer stop the event (issue #7).
fn click_inside(e: &MouseEvent, sel: &str) -> bool {
    e.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| el.closest(sel).ok().flatten())
        .is_some()
}

/// Context budget (tokens) shown by the context bar when the session's
/// model entry carries no `context_tokens` — the kernel's own default
/// (`crates/rushi/src/model_settings.rs`, DEFAULT_CONTEXT_TOKENS).
pub const CTX_BUDGET_FALLBACK: u64 = 262_144;

/// Format a token count the way the context bar reads it: `262144` →
/// "262K", `1048576` → "1M", `1500000` → "1.5M".
pub fn format_ctx(n: u64) -> String {
    if n >= 1_000_000 {
        let m = format!("{:.1}", n as f64 / 1_000_000.0);
        format!("{}M", m.trim_end_matches(".0"))
    } else {
        format!("{}K", n / 1000)
    }
}

// ── three-state layout (M6, persisted) ───────────────────────────
/// Layout modes: "main" (no sidebar — full main view), "split"
/// (240 px sidebar + main), "full" (sidebar expanded to full screen:
/// the multi-session / multi-project dispatch view).
///
/// Migrates the legacy binary `rushi-sidebar-collapsed` key: a stored
/// "1" maps to "main"; anything else falls through to "split".
pub fn read_layout_mode() -> String {
    let stored = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("rushi-layout").ok())
        .flatten();
    if let Some(v) = stored {
        match v.as_str() {
            "main" | "split" | "full" => return v,
            _ => {} // unknown value — fall through to legacy migration
        }
    }
    let legacy = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("rushi-sidebar-collapsed").ok())
        .flatten();
    if legacy.as_deref() == Some("1") {
        "main".to_string()
    } else {
        "split".to_string()
    }
}

fn set_layout_mode(mode: &str) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item("rushi-layout", mode);
    }
}

// ── M8: right tool panel (persisted open state; M11: tabs are
//    connection-scoped, so no per-tab persistence) ─────────────────

pub fn read_rp_open() -> bool {
    let v = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("rushi-rp-open").ok())
        .flatten();
    v.as_deref() == Some("1")
}

fn set_rp_open_stored(open: bool) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item("rushi-rp-open", if open { "1" } else { "0" });
    }
}

/// M8: the right-panel toggle (`#rp-toggle` in the context bar).
pub fn toggle_rp(state: AppState) {
    let open = !state.rp_open.get();
    state.rp_open.set(open);
    set_rp_open_stored(open);
}

// ── theme (v0.5.22: 3-state auto / light / dark, persisted) ────────
/// Read the persisted theme mode. Any value other than "light"/"dark"
/// is normalized to "auto" (the default).
pub fn read_theme_mode() -> String {
    let v = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("rushi-theme").ok())
        .flatten()
        .unwrap_or_else(|| "auto".to_string());
    match v.as_str() {
        "light" => "light".to_string(),
        "dark" => "dark".to_string(),
        _ => "auto".to_string(),
    }
}

fn set_theme_mode_stored(mode: &str) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item("rushi-theme", mode);
    }
}

/// "auto" follows the OS color scheme; light/dark are pinned.
fn effective_theme(mode: &str) -> &str {
    if mode == "auto" {
        // web-sys match_media returns Result<Option<MediaQueryList>, _>
        // (the Option for when matchMedia is unavailable), so flatten.
        let mq = web_sys::window()
            .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok())
            .flatten();
        match mq {
            Some(mq) if mq.matches() => "dark",
            _ => "light",
        }
    } else {
        mode
    }
}

/// Write the effective theme to `<html data-theme>`. The pre-paint
/// inline script in index.html does the same before first paint, so
/// this re-applies the value the page already shows — no flash.
pub fn theme_apply(state: AppState) {
    let mode = state.theme_mode.get(); // owned; lives to the end of the fn
    let eff = effective_theme(&mode);
    if let Some(w) = web_sys::window() {
        if let Some(doc) = w.document() {
            if let Some(html) = doc.document_element() {
                let _ = html.set_attribute("data-theme", eff);
            }
        }
    }
}

/// One-shot theme setup (called from App mount): restore the
/// persisted mode into the signal, apply it, then follow OS
/// color-scheme changes while the mode is "auto". The listener
/// closure keeps its own AppState handle; App lives for the page,
/// so the signal outlives it.
pub fn theme_init(state: AppState) {
    let mode = read_theme_mode();
    state.theme_mode.set(mode.clone());
    theme_apply(state);
    if let Some(mql) = web_sys::window()
        .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok())
        .flatten()
    {
        let cb = Closure::<dyn Fn()>::new(move || {
            if state.theme_mode.get() == "auto" {
                theme_apply(state);
            }
        });
        let f: &js_sys::Function = cb.as_js_value().unchecked_ref();
        let _ = mql.add_listener_with_opt_callback(Some(f));
        cb.forget();
    }
}

/// v0.5.44: fixed 16×16 SVG icons for the theme toggle. Replaces the
/// U+25D0/U+2600/U+263E text glyphs, whose font-dependent metrics made
/// the icon jump in size/baseline between auto/light/dark. All three
/// shapes are drawn on the same 24-unit grid, sized by CSS, and the
/// button box is uniform — cycling the mode no longer changes the
/// button's size or position.
fn theme_icon(mode: &str) -> AnyView {
    match mode {
        "light" => view! {
            <svg class="tt-ic" viewBox="0 0 24 24" aria-hidden="true">
                <circle class="ln" cx="12" cy="12" r="5" />
                <path class="ln" d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42" />
            </svg>
        }
        .into_any(),
        "dark" => view! {
            <svg class="tt-ic" viewBox="0 0 24 24" aria-hidden="true">
                <path class="fl" d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
            </svg>
        }
        .into_any(),
        _ => view! {
            <svg class="tt-ic" viewBox="0 0 24 24" aria-hidden="true">
                <circle class="ln" cx="12" cy="12" r="9" />
                <path class="fl" d="M12 3a9 9 0 0 0 0 18Z" />
            </svg>
        }
        .into_any(),
    }
}

/// v0.5.42: the model-settings button icon (sliders), same 16×16
/// stroke style as the theme icons.
fn model_settings_icon() -> AnyView {
    view! {
        <svg class="tt-ic" viewBox="0 0 24 24" aria-hidden="true">
            <path class="ln" d="M4 6h16M4 12h16M4 18h16" />
            <circle class="fl" cx="9" cy="6" r="2" />
            <circle class="fl" cx="15" cy="12" r="2" />
            <circle class="fl" cx="7" cy="18" r="2" />
        </svg>
    }
    .into_any()
}

/// v0.5.44: the session card's model chip — the entry this session runs
/// with, plus a popup styled after the input box's steer selector to
/// change it. The chip is disabled while the session's loop runs: the
/// choice applies at the next launch, so changing it mid-run would only
/// mislead (and, worse, make the chip disagree with what is running).
#[component]
fn SessionModelChip(
    state: AppState,
    name: String,
    model: Option<String>,
    running: Signal<bool>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    // The popup is positioned `fixed` at the click point (like #sess-menu):
    // the card lives in `#session-list`, whose `overflow-y:auto` would
    // clip an absolutely positioned panel.
    let pos = RwSignal::new((0.0_f64, 0.0_f64));
    let names = state.model_names;
    let sessions = state.sessions;
    // Signals, not plain Strings: a `move` closure nested in the `view!`
    // body would otherwise move them out of the view closure, which must
    // stay `Fn`.
    let session_name = RwSignal::new(name);
    // v0.5.46: the label reads back from the session list instead of the
    // mount-time prop (issue #6). The popup reloads `state.sessions`
    // after a change, so the chip follows the new entry without a page
    // reload; the prop is only the seed for a row the list lacks yet.
    let initial = model.unwrap_or_default();
    let current = Signal::derive(move || {
        let n = session_name.get();
        let picked = sessions
            .get()
            .into_iter()
            .find(|s| s.name == n)
            .and_then(|s| s.model);
        match picked {
            Some(m) if !m.is_empty() => m,
            _ => initial.clone(),
        }
    });
    let label = move || {
        let c = current.get();
        if c.is_empty() {
            "auto".to_string()
        } else {
            c
        }
    };
    let title = move || {
        let l = label();
        if running.get() {
            format!("model: {l} — locked while the loop runs")
        } else {
            format!("model: {l} — click to change")
        }
    };

    view! {
        <div class="sess-model">
            <button
                class="sess-model-chip"
                disabled=move || running.get()
                title=title
                on:click=move |e: MouseEvent| {
                    e.stop_propagation();
                    // Open above the chip, or below when it sits too high.
                    let y = if e.client_y() as f64 > 260.0 {
                        e.client_y() as f64 - 8.0
                    } else {
                        e.client_y() as f64 + 22.0
                    };
                    pos.set((e.client_x() as f64, y));
                    open.update(|o| *o = !*o);
                }
            >
                <span class="smc-text">{ move || label() }</span>
                <span class="qsel-chev">{ "\u{25be}" }</span>
            </button>
            <Show when=move || open.get() fallback=|| ()>
                <div
                    class="qsel-backdrop"
                    on:click=move |e: MouseEvent| {
                        e.stop_propagation();
                        open.set(false);
                    }
                />
                <div
                    class="qsel-panel sess-model-panel"
                    style=move || {
                        let (x, y) = pos.get();
                        format!("left:{x:.0}px; top:{y:.0}px;")
                    }
                    on:click=move |e: MouseEvent| e.stop_propagation()
                >
                    { move || {
                        let cur = current.get();
                        let session_base = session_name.get();
                        let mut opts: Vec<AnyView> = names
                            .get()
                            .into_iter()
                            .map(|n| {
                                let sel = n == cur;
                                let value = n.clone();
                                let session = session_base.clone();
                                let st = state;
                                view! {
                                    <button
                                        class="qsel-opt"
                                        aria-selected=sel
                                        on:click=move |_| {
                                            let session = session.clone();
                                            let value = value.clone();
                                            spawn_local(async move {
                                                let _ = api::set_session_model(
                                                    &session,
                                                    Some(value.as_str()),
                                                )
                                                    .await;
                                                if let Ok(list) = api::load_sessions().await {
                                                    st.sessions.set(list);
                                                }
                                            });
                                            // v0.5.46: close on the next
                                            // macrotask. Closing inside the
                                            // handler unmounts the panel
                                            // mid-dispatch, which drops the
                                            // panel's own stop_propagation
                                            // listener — the click then
                                            // reaches the session card and
                                            // switches sessions (issue #7).
                                            after_dispatch(move || open.set(false));
                                        }
                                    >
                                        <span class="qsel-tick">{ "\u{2713}" }</span>
                                        { n.clone() }
                                    </button>
                                }
                                .into_any()
                            })
                            .collect();
                        opts
                    } }
                </div>
            </Show>
        </div>
    }
}

// ── sidebar ordering (v0.5.30: created / output / custom) ─────────
/// Read the persisted ordering mode + custom order. Any value other
/// than "created"/"output"/"custom" normalizes to "output" (the
/// default, which preserves the pre-v0.5.30 "by latest output"
/// behavior at loop granularity).
pub fn read_persisted_sort() -> (String, Vec<String>) {
    let stored = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten();
    let mode = stored
        .as_ref()
        .and_then(|s| s.get_item("rushi-sort-mode").ok().flatten())
        .unwrap_or_else(|| "output".to_string());
    let mode = match mode.as_str() {
        "created" | "output" | "custom" => mode,
        _ => "output".to_string(),
    };
    let order = stored
        .and_then(|s| s.get_item("rushi-custom-order").ok().flatten())
        .and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok())
        .unwrap_or_default();
    (mode, order)
}

pub fn persist_sort_mode(mode: &str) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item("rushi-sort-mode", mode);
    }
}

pub fn persist_custom_order(order: &[String]) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item(
            "rushi-custom-order",
            &serde_json::to_string(order).unwrap_or_else(|_| "[]".into()),
        );
    }
}

/// v0.5.40: drop `drag` before `target` (None = end of the list) in the
/// custom order.
///
/// `custom_order` is kept a FULL permutation of the session names, so
/// `ordered_sessions` renders exactly the stored order. The v0.5.30
/// variant stored only the dragged names, so an undragged card fell to
/// the name-ordered tail and a drop landed in the wrong place (on a
/// target it moved the card near the top; the end drop zone moved it
/// to the top instead of appending). Now:
/// - the effective order starts from the current custom-mode order,
///   pruned to live sessions, with any new sessions appended;
/// - the dragged name moves to just before `target`, or to the end
///   when `target` is None;
/// - the result is persisted.
pub fn reorder_sessions(state: AppState, drag: &str, target: Option<&str>) {
    let sessions = state.sessions.get();
    let names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
    if !names.iter().any(|n| n == drag) {
        return;
    }
    let mut order: Vec<String> = ordered_sessions(
        &sessions,
        "custom",
        &state.custom_order.get(),
        &state.output_rank.get(),
    )
    .into_iter()
    .map(|s| s.name)
    .filter(|n| names.iter().any(|m| m == n))
    .collect();
    for n in &names {
        if !order.iter().any(|x| x == n) {
            order.push(n.clone());
        }
    }
    order.retain(|n| n != drag);
    let at = match target {
        Some(t) => order.iter().position(|n| n == t).unwrap_or(order.len()),
        None => order.len(),
    };
    order.insert(at, drag.to_string());
    state.custom_order.set(order.clone());
    persist_custom_order(&order);
}

/// v0.5.30: carry the ordering bookkeeping across a rename: the
/// custom order and the output rank keep working under the new name.
pub fn rename_session_order(state: AppState, old: &str, new: &str) {
    state.custom_order.update(|order| {
        if let Some(i) = order.iter().position(|n| n == old) {
            order[i] = new.to_string();
        }
    });
    state.output_rank.update(|rank| {
        if let Some(v) = rank.remove(old) {
            rank.insert(new.to_string(), v);
        }
    });
    persist_custom_order(&state.custom_order.get());
}

// ── session selection / mutation helpers ──────────────────────────
pub fn select_session(state: AppState, name: &str) {
    // v0.5.13: leaving a session consumes its "loop finished, not
    // viewed since" green bar — the lamp stays while the session is
    // viewed and is released only when the user switches away.
    if state.active_session.get().as_deref() != Some(name) {
        if let Some(old) = state.active_session.get() {
            state.loop_done_unviewed.write().remove(&old);
        }
    }
    state.active_session.set(Some(name.to_string()));
    state.events.set(Vec::new());
    state.view_round.set(None);
    state.ctx_used.set(0);
    state.rounds_ctxk.set(Vec::new());
    state.goal.set(None);
    // v0.5.38: drop the previous session's loop-cmd value so the chip
    // does not briefly show the OTHER session's command while the new
    // session's full-transcript fetch is in flight.
    state.loop_cmd.set(String::new());
    state.loop_cmd_sess.set(None);
    state.menu_session.set(None);
    state.clear_live();
    reset_panel_session(state);

    let s2 = state;
    let name2 = name.to_string();
    spawn_local(async move {
        if let Ok(sessions) = api::load_sessions().await {
            s2.sync_session_bookkeeping(&sessions);
            s2.sessions.set(sessions);
        }
        ws::connect(&s2, &name2);
        s2.goal.set(api::load_goal(&name2).await);
        s2.loop_running.set(api::loop_running(&name2).await);
        // v0.5.38: fetch the full transcript right away so the loop-cmd
        // chip is correct from the first paint (not after the 4s poll),
        // when its command is buried under >200 model/tool events.
        if s2.loop_running.get() {
            if let Ok(evs) = api::load_events(&name2).await {
                s2.loop_cmd.set(crate::model::last_user_command_slice(&evs));
                s2.loop_cmd_sess.set(Some(name2.clone()));
            }
        }
    });
}

/// M11: the right-panel tab model is connection-scoped — a session
/// switch (or deletion of the active session) tears down every open
/// tab. The server killed the previous socket's ptys on disconnect, so
/// close all terminal tabs' ptys explicitly (a no-op for already-dead
/// ids), detach every xterm writer, and reset to a bare Files home tab.
fn reset_panel_session(state: AppState) {
    for t in state.rp_tabs.get().iter().filter(|t| t.kind == RpTabKind::Term) {
        ws::term_close(t.id);
    }
    ws::clear_all_term_writers();
    state.rp_tabs.set(vec![RpTab {
        id: 0,
        kind: RpTabKind::Files,
        path: String::new(),
        label: "Files".to_string(),
    }]);
    state.rp_active.set(0);
    state.rp_next_id.set(1);
    state.rp_term_seq.set(1);
    state.rp_expanded.update(|s| s.clear());
    state.rp_children.set(std::collections::HashMap::new());
    state.rp_preview_map.set(std::collections::HashMap::new());
    state.rp_err_map.set(std::collections::HashMap::new());
    state.term_state.set(std::collections::HashMap::new());
}

pub fn delete_session(state: AppState, name: &str) {
    let name_owned = name.to_string();
    let s2 = state;
    spawn_local(async move {
        match api::delete_session(&name_owned).await {
            Ok(()) => {
                if s2.active_session.get().as_deref() == Some(name_owned.as_str()) {
                    s2.active_session.set(None);
                    s2.events.set(Vec::new());
                    s2.view_round.set(None);
                    s2.ctx_used.set(0);
                    s2.rounds_ctxk.set(Vec::new());
                    s2.goal.set(None);
                    s2.clear_live();
                    reset_panel_session(s2);
                    ws::close_current();
                    s2.ws_status.set("disconnected".to_string());
                }
                if let Ok(sessions) = api::load_sessions().await {
                    s2.sync_session_bookkeeping(&sessions);
                    s2.sessions.set(sessions);
                }
            }
            Err(e) => {
                if let Some(w) = web_sys::window() {
                    let _ = w.alert_with_message(&format!("delete failed: {e}"));
                }
            }
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
        // v0.5.30: carry the ordering bookkeeping across the rename.
        rename_session_order(state2, &old_owned, &new_owned);
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

// v0.5.6: start_loop lived here for the sidebar start button; the
// loop start is now folded into do_send (ensure-loop-running), so
// the standalone function is gone. stop_loop remains (the send
// button's red-square state calls it).

pub fn stop_loop(state: AppState) {
    let Some(id) = state.active_session.get() else { return };
    if ws::is_open() {
        ws::send_command(&id, &json!({ "kind": "stop" }));
    } else {
        let _id = id.clone();
        spawn_local(async move {
            let _ = api::stop_loop(&_id).await;
        });
    }
    state.loop_running.set(false);
}

/// M7: dispatch-view quick action — start/stop the loop of ANY
/// session (not just the active one) over REST. The server's
/// loop_status frames update the per-session lamps; if the target is
/// the active session, mirror the local send-button flag too.
pub fn dispatch_loop_action(state: AppState, name: &str, start: bool) {
    let active = state.active_session.get();
    if active.as_deref() == Some(name) {
        state.loop_running.set(start);
    }
    let name_owned = name.to_string();
    spawn_local(async move {
        let r = if start {
            api::start_loop(&name_owned).await
        } else {
            api::stop_loop(&name_owned).await
        };
        if let Err(e) = r {
            if let Some(w) = web_sys::window() {
                let _ = w.alert_with_message(&format!(
                    "{} loop for {} failed: {e}",
                    if start { "start" } else { "stop" },
                    name_owned
                ));
            }
        }
    });
}

/// M7: the last-modified time for dispatch cards (same 0-based HH:MM:SS
/// label as the flat sidebar list; "no events" when never written).
fn dispatch_time_label(ts: Option<f64>) -> String {
    ts.map(|t| {
        let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(t * 1000.0));
        format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds())
    })
    .unwrap_or_else(|| "no events".to_string())
}

/// M7: one project group in the dispatch view (layout "full"): a header
/// line (project directory, full path in the title, card count) plus one
/// card per session. The group's membership/order is baked into the For
/// key (see Sidebar), so the group re-renders when a session joins,
/// leaves, or reorders within the project.
fn dispatch_group_block(
    state: AppState,
    group_key: String,
    group_sessions: Vec<SessionInfo>,
) -> impl IntoView {
    let label = dispatch_group_label(&group_key);
    let count = group_sessions.len();
    view! {
        <div class="dispatch-group">
            <div class="dispatch-group-head" title={group_key.clone()}>
                { label.clone() }
                <span class="dispatch-group-count">{ count }</span>
            </div>
            <For
                each=move || group_sessions.clone()
                key=|s: &SessionInfo| s.name.clone()
                children=move |s| dispatch_card(state, s.clone())
            />
        </div>
    }
}

/// M7: a single session card in the dispatch view: name + last-output
/// time on the top line, and a start/stop toggle plus the session menu
/// on the action line. Clicking the card enters the session and returns
/// to the "split" layout.
fn dispatch_card(state: AppState, s: SessionInfo) -> impl IntoView {
    let name = s.name.clone();
    let ts = s.last_modified;

    let active = state.active_session;
    let loop_running = state.loop_running;
    let looping_set = state.looping_sessions;
    let done_set = state.loop_done_unviewed;
    let menu_session = state.menu_session;
    let menu_pos = state.menu_pos;
    let layout = state.layout_mode;

    // One owned clone per `move` handler (same idiom as the flat
    // list): each handler owns its copy and only ever *borrows* it,
    // keeping every `on:` handler an `Fn` closure.
    let name_cls = name.clone();
    let name_click = name.clone();
    let name_qa_cls = name.clone(); // quick-action button class
    let name_qa_lbl = name.clone(); // quick-action button label
    let name_qa_ck = name.clone(); // quick-action button click
    let name_menu = name.clone();
    let name_view = name.clone();
    let name_title = name.clone();

    let card_cls = move || {
        let n = name_cls.as_str();
        let active_now = active.get().as_deref() == Some(n);
        let looping_now = looping_set.get().contains(n)
            || (active_now && loop_running.get());
        let done_now = done_set.get().contains(n);
        let mut c = String::from("dispatch-card");
        if active_now {
            c.push_str(" active");
        }
        if looping_now {
            c.push_str(" running");
        }
        if done_now && !looping_now {
            c.push_str(" done");
        }
        c
    };

    // The quick action is a single toggle button: "▶ start" while this
    // session's loop is stopped, "■ stop" while it runs. For the
    // active session the live flag is `loop_running`; all others use
    // the server-driven `looping_sessions` set.
    let qa_cls = move || {
        let n = name_qa_cls.as_str();
        let running = looping_set.get().contains(n)
            || (active.get().as_deref() == Some(n) && loop_running.get());
        if running {
            "qa qa-stop".to_string()
        } else {
            "qa qa-go".to_string()
        }
    };
    let qa_label = move || {
        let n = name_qa_lbl.as_str();
        let running = looping_set.get().contains(n)
            || (active.get().as_deref() == Some(n) && loop_running.get());
        if running {
            "\u{25A0}  stop".to_string()
        } else {
            "\u{25B6}  start".to_string()
        }
    };

    view! {
        <div
            class=card_cls
            on:click=move |_| {
                // M7: entering a session from the dispatch view returns
                // to the split layout.
                select_session(state, &name_click);
                menu_session.set(None);
                if layout.get() == "full" {
                    layout.set("split".to_string());
                    set_layout_mode("split");
                }
            }
        >
            <div class="dc-top">
                <span class="dc-name" title={name_title}>{ name_view }</span>
                <span class="dc-time">{ dispatch_time_label(ts) }</span>
            </div>
            <div class="dc-actions">
                <button
                    class=qa_cls
                    title="start / stop this session's loop"
                    on:click=move |e: MouseEvent| {
                        e.stop_propagation();
                        let n = name_qa_ck.as_str();
                        let running = looping_set.get().contains(n)
                            || (active.get().as_deref() == Some(n) && loop_running.get());
                        dispatch_loop_action(state, &name_qa_ck, !running);
                    }
                >
                    { qa_label }
                </button>
                <button
                    class="sess-more"
                    title="session actions"
                    on:click=move |e: MouseEvent| {
                        e.stop_propagation();
                        menu_pos.set((
                            e.client_x() as f64,
                            e.client_y() as f64,
                        ));
                        menu_session.set(Some(name_menu.clone()));
                    }
                >
                    { "\u{2026}" }
                </button>
            </div>
        </div>
    }
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
    // Only `follow` writes the queue field; the steer path leaves it
    // absent (schema: a missing field means steer, old logs stay valid).
    let queue_opt = (queue == "follow").then(|| queue.clone());

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
    // v0.5.48: sending IS "watch this round". Re-arm the follow here, at
    // the one place the user's intent is unambiguous — the trigger
    // detection in the engine sees the same `user_message` type the loop
    // injects, so it has to stay gated on a strict reading it can see
    // (v0.5.34). Without this, sending from a read-up position never
    // came back to the bottom and never followed the round's output.
    crate::pile::note_user_send();

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
                // The server's loop_status frame flips the flag.
                ws::send_command(&active3, &json!({ "kind": "start" }));
            } else {
                match api::start_loop(&active3).await {
                    Ok(()) => state3.loop_running.set(true),
                    Err(e) => {
                        if let Some(w) = web_sys::window() {
                            let _ = w.alert_with_message(&format!(
                                "start loop failed: {e}"
                            ));
                        }
                    }
                }
            }
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
    let layout = state.layout_mode;

    // The menu state lives in AppState so the ported document-level
    // click/Escape handlers (pile::init) can close it.
    let menu_session = state.menu_session;
    let menu_pos = state.menu_pos;

    // v0.5.30: sidebar ordering — mode + custom order + the "output"
    // rank map (bumped only on loop completion) + transient drag
    // state (custom mode).
    let sort_mode = state.sort_mode;
    let custom_order = state.custom_order;
    let output_rank = state.output_rank;
    let dragging_session = state.dragging_session;
    let drop_target = state.drop_target;

    let ws_dot_class = move || {
        let s = ws_status.get();
        match s.as_str() {
            "connected" => "ws-dot connected".to_string(),
            "connecting" => "ws-dot connecting".to_string(),
            "error" => "ws-dot error".to_string(),
            _ => "ws-dot disconnected".to_string(),
        }
    };

    // v0.5.22: theme cycle button (auto → light → dark → auto).
    let theme_mode = state.theme_mode;
    let theme_title = move || match theme_mode.get().as_str() {
        "light" => "theme: light — click for dark".to_string(),
        "dark" => "theme: dark — click for auto".to_string(),
        _ => "theme: auto (follows system) — click for light".to_string(),
    };

    // M6: layout is driven entirely by the `#app.layout-*` class
    // (lib.rs); the aside itself carries no state class.
    // (`layout` is bound at the top of this function.)

    view! {
        <aside id="sidebar">
            <div id="sidebar-inner">
                <div class="sb-header">
                    <h1>
                        { "Rushi" }
                        // v0.5.40: build version, injected at build
                        // time (web-leptos/build.rs) — single source of
                        // truth with the console marker.
                        <span class="sb-version">{ WEBUI_VERSION }</span>
                    </h1>
                    <div class="sb-actions">
                        <button
                            id="theme-toggle"
                            title=theme_title
                            on:click=move |_| {
                                // auto -> light -> dark -> auto
                                let next = match theme_mode.get().as_str() {
                                    "auto" => "light",
                                    "light" => "dark",
                                    _ => "auto",
                                };
                                theme_mode.set(next.to_string());
                                set_theme_mode_stored(next);
                                theme_apply(state);
                            }
                        >
                            { move || theme_icon(theme_mode.get().as_str()) }
                        </button>
                        // M6: the sidebar is visible in "split" and "full"
                        // (hidden in "main", where the ContextBar's » button
                        // brings it back). The collapse chevron is state-
                        // dependent: from "split" it hides the sidebar
                        // (→ "main"); from "full" it returns to "split".
                        <Show when=move || layout.get() != "main" fallback=|| ()>
                            <button
                                id="sidebar-toggle"
                                title=move || {
                                    if layout.get() == "full" {
                                        "back to split view".to_string()
                                    } else {
                                        "hide sidebar".to_string()
                                    }
                                }
                                on:click=move |_| {
                                    let next = if layout.get() == "full" {
                                        "split"
                                    } else {
                                        "main"
                                    };
                                    layout.set(next.to_string());
                                    set_layout_mode(next);
                                }
                            >
                                { "\u{2039}" }
                            </button>
                        </Show>
                        // M6: the expand chevron (symmetric to the collapse
                        // one) grows the sidebar to the full-screen
                        // dispatch view. Only offered from "split".
                        <Show when=move || layout.get() == "split" fallback=|| ()>
                            <button
                                id="sidebar-expand"
                                title="expand sidebar to full screen"
                                on:click=move |_| {
                                    layout.set("full".to_string());
                                    set_layout_mode("full");
                                }
                            >
                                { "\u{203A}" }
                            </button>
                        </Show>
                    </div>
                </div>
                <div id="session-actions">
                    <button
                        id="btn-new"
                        on:click=move |_| { new_session(state); }
                    >
                        { "+ New Session" }
                    </button>
                    // v0.5.30: ordering mode — created / output / custom.
                    // "output" moves the order only when a loop
                    // COMPLETES; "custom" enables card drag & drop.
                    <div class="sb-seg" title="session order">
                        <button
                            class=move || if sort_mode.get() == "created" { "on" } else { "" }
                            title="by creation time — newest on top"
                            on:click=move |_| {
                                sort_mode.set("created".to_string());
                                persist_sort_mode("created");
                            }
                        >
                            { "newest" }
                        </button>
                        <button
                            class=move || if sort_mode.get() == "output" { "on" } else { "" }
                            title="by last output — the order updates when a loop completes"
                            on:click=move |_| {
                                sort_mode.set("output".to_string());
                                persist_sort_mode("output");
                            }
                        >
                            { "output" }
                        </button>
                        <button
                            class=move || if sort_mode.get() == "custom" { "on" } else { "" }
                            title="your order — drag the cards to arrange them"
                            on:click=move |_| {
                                if sort_mode.get() == "custom" {
                                    return;
                                }
                                // v0.5.40: entering custom mode for the
                                // first time freezes the CURRENT visible
                                // order (of the mode being left) as the
                                // starting custom order, so the list does
                                // not jump to the name-ordered fallback.
                                if custom_order.get().is_empty() {
                                    let s = sessions.get();
                                    let leaving = sort_mode.get();
                                    let seed: Vec<String> =
                                        ordered_sessions(
                                            &s,
                                            &leaving,
                                            &custom_order.get(),
                                            &output_rank.get(),
                                        )
                                        .into_iter()
                                        .map(|x| x.name)
                                        .collect();
                                    custom_order.set(seed.clone());
                                    persist_custom_order(&seed);
                                }
                                sort_mode.set("custom".to_string());
                                persist_sort_mode("custom");
                            }
                        >
                            { "custom" }
                        </button>
                    </div>
                </div>
                <div
                    id="session-list"
                    // v0.5.30: highlight the empty area as a drop
                    // target (append to end) while a card is dragged
                    // and the pointer is not over a session item.
                    class=move || {
                        if dragging_session.get().is_some()
                            && drop_target.get().is_none()
                        {
                            "drop-end".to_string()
                        } else {
                            String::new()
                        }
                    }
                    // v0.5.30: end-of-list drop zone (custom mode):
                    // dropping into the empty area appends to the end.
                    on:dragover=move |e: DragEvent| {
                        if dragging_session.get().is_some() {
                            e.prevent_default();
                        }
                    }
                    on:drop=move |e: DragEvent| {
                        let Some(drag) = dragging_session.get() else {
                            return;
                        };
                        e.prevent_default();
                        reorder_sessions(state, &drag, None);
                        dragging_session.set(None);
                        drop_target.set(None);
                    }
                >
                    // M7: the dispatch view (layout "full") — sessions
                    // grouped by project (cwd), each card carrying
                    // start/stop quick actions. The flat list below stays
                    // the split/main rendering.
                    <Show
                        when=move || layout.get() == "full"
                        fallback=|| ()
                    >
                        <For
                            each=move || {
                                let s = sessions.get();
                                let m = sort_mode.get();
                                let o = custom_order.get();
                                let r = output_rank.get();
                                dispatch_groups(&s, &m, &o, &r)
                            }
                            // key includes the member order so any
                            // reordering within a project re-renders the
                            // group.
                            key=|g: &(String, Vec<SessionInfo>)| {
                                let mut k = g.0.clone();
                                k.push_str("::");
                                for item in &g.1 {
                                    k.push_str(&item.name);
                                    k.push(',');
                                }
                                k
                            }
                            children=move |g| {
                                dispatch_group_block(state, g.0.clone(), g.1.clone())
                            }
                        />
                    </Show>
                    <Show
                        when=move || layout.get() != "full"
                        fallback=|| ()
                    >
                    <For
                        each=move || {
                            let s = sessions.get();
                            let m = sort_mode.get();
                            let o = custom_order.get();
                            let r = output_rank.get();
                            ordered_sessions(&s, &m, &o, &r)
                        }
                        key=|s: &SessionInfo| s.name.clone()
                        children=move |s| {
                            let s_name = s.name.clone();
                            let s_ts = s.last_modified;
                            let s_model = s.model.clone();
                            // v0.5.44: the chip's own clones (its name goes
                            // into a derived signal, its model into the
                            // component).
                            let chip_name = s_name.clone();
                            let chip_signal_name = s_name.clone();
                            // One owned clone per `move` handler: a String
                            // moves into only one closure, and the click +
                            // three drag handlers each capture it.
                            let click_name = s_name.clone();
                            let click_name_start = s_name.clone();
                            let click_name_over = s_name.clone();
                            let click_name_drop = s_name.clone();
                            let menu_name = s_name.clone();
                            let item_cls_name = s_name.clone();
                            let looping_set = state.looping_sessions;
                            let done_set = state.loop_done_unviewed;
                            let item_cls = move || {
                                let mut c = String::from("session-item");
                                let is_active =
                                    active.get().as_deref() == Some(item_cls_name.as_str());
                                // v0.5.13: server-driven per-session loop
                                // state. A session looping in the
                                // background gets its breathing lamp on
                                // a relief card in the background color;
                                // a finished-but-unviewed loop shows a
                                // static green bar.
                                let looping = looping_set.get().contains(item_cls_name.as_str());
                                let done = done_set.get().contains(item_cls_name.as_str());
                                if is_active {
                                    c.push_str(" active");
                                    if looping || loop_running.get() {
                                        c.push_str(" running");
                                    }
                                } else if looping {
                                    c.push_str(" running running-bg");
                                }
                                if done && !looping {
                                    c.push_str(" done");
                                }
                                // v0.5.30: transient drag state (custom
                                // mode): the card being dragged fades;
                                // the item under the cursor marks the
                                // drop position.
                                if dragging_session
                                    .get()
                                    .as_deref()
                                    == Some(item_cls_name.as_str())
                                {
                                    c.push_str(" dragging");
                                }
                                if drop_target
                                    .get()
                                    .as_deref()
                                    == Some(item_cls_name.as_str())
                                {
                                    c.push_str(" drop-target");
                                }
                                c
                            };
                            view! {
                                <div
                                    class=item_cls
                                    // v0.5.40: `draggable` is a LIMITED
                                    // boolean attribute — its VALUE
                                    // matters ("true"/"false"), not just
                                    // presence. The v0.5.30 plain-bool
                                    // binding made Leptos emit
                                    // draggable="" for true, which this
                                    // Chromium does not treat as
                                    // draggable, so a native drag never
                                    // started (the "ineffective drag" bug).
                                    // Bind an explicit string value.
                                    draggable=move || {
                                        if sort_mode.get() == "custom" {
                                            "true".to_string()
                                        } else {
                                            "false".to_string()
                                        }
                                    }
                                    on:click=move |e: MouseEvent| {
                                        // v0.5.46: the model chip, its
                                        // popup and its backdrop own their
                                        // clicks — a click of theirs must
                                        // never switch sessions (issue #7).
                                        if click_inside(&e, ".sess-model") {
                                            return;
                                        }
                                        select_session(state, &click_name);
                                        menu_session.set(None);
                                    }
                                    on:dragstart=move |e: DragEvent| {
                                        if sort_mode.get() != "custom" {
                                            return;
                                        }
                                        // setData is required for the
                                        // browser to start the drag.
                                        if let Some(dt) = e.data_transfer() {
                                            let _ = dt.set_data("text/plain", &click_name_start);
                                        }
                                        dragging_session.set(Some(click_name_start.clone()));
                                    }
                                    on:dragover=move |e: DragEvent| {
                                        let Some(drag) = dragging_session.get() else {
                                            return;
                                        };
                                        // v0.5.40: always prevent_default
                                        // while a drag is in progress, even
                                        // over the dragged card itself. The
                                        // v0.5.30 early-return made the
                                        // browser mark a no-drop cursor on
                                        // the source card and abort a
                                        // release there.
                                        e.prevent_default();
                                        if drag != click_name_over {
                                            drop_target.set(Some(click_name_over.clone()));
                                        }
                                    }
                                    on:drop=move |e: DragEvent| {
                                        let Some(drag) = dragging_session.get() else {
                                            return;
                                        };
                                        // Keep the list-level drop
                                        // zone from re-firing.
                                        e.stop_propagation();
                                        e.prevent_default();
                                        if drag != click_name_drop {
                                            reorder_sessions(state, &drag, Some(&click_name_drop));
                                        }
                                        dragging_session.set(None);
                                        drop_target.set(None);
                                    }
                                    on:dragend=move |_| {
                                        // Drop failed (left the list) or
                                        // completed: always clear.
                                        dragging_session.set(None);
                                        drop_target.set(None);
                                    }
                                >
                                    <span class="sname">{ s_name }</span>
                                    // v0.5.48: no timestamp on the
                                    // sidebar card (the transcript
                                    // cards carry the full date + time
                                    // now). Only the "no events" state
                                    // stays — it is information, not a
                                    // time.
                                    <Show
                                        when=move || s_ts.is_none()
                                        fallback=|| ()
                                    >
                                        <span class="smeta">{"no events"}</span>
                                    </Show>
                                    <SessionModelChip
                                        state=state
                                        name=chip_name
                                        model=s_model
                                        running=Signal::derive(move || {
                                            let n = chip_signal_name.as_str();
                                            state.looping_sessions.get().contains(n)
                                                || (state.active_session.get().as_deref()
                                                    == Some(n)
                                                    && state.loop_running.get())
                                        })
                                    />
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
                    </Show>
                </div>
                <GoalPanel state=state />
                <div id="status-bar">
                    <span class=ws_dot_class></span>
                    { move || ws_status.get() }
                    <Show when=move || active.get().is_some()>
                        <span class="sb-loop">
                            { move || {
                                if loop_running.get() {
                                    "loop running".to_string()
                                } else {
                                    "loop stopped".to_string()
                                }
                            } }
                        </span>
                    </Show>
                    // v0.5.44: model settings, pinned to the sidebar's
                    // bottom-right (margin-left:auto in CSS).
                    <button
                        id="model-settings"
                        title="model settings"
                        on:click=move |_| state.model_open.set(true)
                    >
                        { model_settings_icon() }
                    </button>
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
                                // Defer the close to a macrotask: this Chromium
                                // build drains microtasks mid-dispatch, so a
                                // spawn_local unmount would drop the menu's
                                // stop-propagation closure before the bubble
                                // phase finishes and throw "closure invoked
                                // after being dropped".
                                after_dispatch(move || {
                                    if !name.is_empty() {
                                        rename_session(state, &name);
                                    }
                                    menu_session.set(None);
                                });
                            }
                        >
                            { "rename" }
                        </button>
                        <button
                            class="sess-menu-item danger"
                            on:click=move |_| {
                                let name = menu_session.get().unwrap_or_default();
                                after_dispatch(move || {
                                    if !name.is_empty() {
                                        state.confirm_delete.set(Some(name));
                                    }
                                    menu_session.set(None);
                                });
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
    let layout = state.layout_mode;

    // v0.5.46: the budget is the ACTIVE session's model entry, not a
    // hard-coded 262k — a 1M-context entry reads "1M". The server
    // resolves each session's model (own choice -> last used -> config
    // active), so the name is always there; a name with no matching
    // entry (renamed away, deleted) falls back to the kernel's default.
    let budget = move || {
        let Some(sess) = state.active_session.get() else {
            return CTX_BUDGET_FALLBACK;
        };
        let name = state
            .sessions
            .get()
            .into_iter()
            .find(|s| s.name == sess)
            .and_then(|s| s.model)
            .unwrap_or_default();
        if name.is_empty() {
            return CTX_BUDGET_FALLBACK;
        }
        state
            .model_ctx
            .get()
            .get(&name)
            .copied()
            .unwrap_or(CTX_BUDGET_FALLBACK)
    };

    let fill_style = move || {
        let used = ctx_used.get();
        let pct = (used as f64 / budget() as f64 * 100.0).min(100.0);
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
        let pct = (ctx_used.get() as f64 / budget() as f64 * 100.0).min(100.0);
        format!("{pct:.1}%")
    };

    let ctx_k = move || format!("~{} / {}", format_ctx(ctx_used.get()), format_ctx(budget()));

    view! {
        <div id="context-bar">
            <div id="ctx-row">
                <Show when=move || layout.get() == "main" fallback=|| ()>
                    <button
                        id="sidebar-open"
                        title="expand sidebar"
                        on:click=move |_| {
                            layout.set("split".to_string());
                            set_layout_mode("split");
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
                // M8: right tool panel toggle (Files tree / preview /
                // terminal). Right edge of the context bar, symmetric to
                // the sidebar's own edge buttons.
                <button
                    id="rp-toggle"
                    title=move || {
                        if state.rp_open.get() {
                            "close the right panel".to_string()
                        } else {
                            "open the right panel".to_string()
                        }
                    }
                    on:click=move |_| toggle_rp(state)
                >
                    { move || if state.rp_open.get() { "▣" } else { "▢" } }
                </button>
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

    // v0.5.21: global round numbering. When the loaded window is
    // truncated (has_more), the chips cover only the window's rounds;
    // the server-reported total_rounds gives each chip its TRUE
    // position in the full log: window round i (0-based) is global
    // round (total_rounds - n) + i + 1. With the full log loaded,
    // total_rounds == n and the labels collapse to 1..n as before.
    // 0 = unknown (server without the field) → plain window numbering.
    let total_rounds = state.hist_total_rounds.get();
    let base = total_rounds.saturating_sub(n as u64);

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
            lines.push(format!("round {}{k_part}", base + i as u64 + 1));
            let title = lines.join("\n");
            let i2 = i;
            let vr = view_round;
            let ra = state.round_active;
            let chip_cls = move || {
                let mut c = String::from("ctx-round");
                // Pile mode: view_round drives the highlight (round
                // filter). Flat mode: round_active (the scroll position)
                // drives it — same class, so both read one look.
                if vr.get() == Some(i2) || ra.get() == Some(i2) {
                    c.push_str(" on");
                }
                c
            };
            view! {
                <button
                    class=chip_cls
                    title=title.clone()
                    on:click=move |_| {
                        // v0.5.8: mode-aware — flat mode glides the
                        // transcript to this round; pile mode keeps the
                        // legacy round-filter semantics (view_round).
                        crate::pile::nav_to_round(i2);
                    }
                />
            }.into_any()
        } else {
            view! { <button class="ctx-round ctx-spot" disabled=true /> }.into_any()
        }
    }).collect();

    // v0.5.21: leftmost "⋯ N earlier" chip — how many rounds exist in
    // the full log but are not in the loaded window yet. Clicking it
    // pages backwards like the transcript's load-earlier pill. Shown
    // only while older pages exist AND the total is known; hidden in
    // single-round pin view (the pill is hidden there too).
    let earlier: AnyView = {
        let more = total_rounds.saturating_sub(n as u64);
        view! {
            <Show
                when=move || {
                    state.hist_has_more.get()
                        && state.hist_total_rounds.get() > 0
                        && view_round.get().is_none()
                }
                fallback=|| ()
            >
                <button
                    class=move || {
                        if state.earlier_failed.get() { "ctx-round-more failed" } else { "ctx-round-more" }
                    }
                    title=format!("{more} earlier rounds not loaded \u{b7} click to load")
                    disabled=move || state.loading_earlier.get()
                    on:click=move |_| crate::ws::load_earlier(&state)
                >
                    { move || {
                        if state.loading_earlier.get() {
                            "\u{22ef}\u{2026}".to_string()
                        } else {
                            let m = state.hist_total_rounds.get().saturating_sub(
                                compute_rounds(&events.get()).len() as u64,
                            );
                            format!("\u{22ef}{m}")
                        }
                    } }
                </button>
            </Show>
        }
    }
    .into_any();

    let mut all = Vec::with_capacity(chips.len() + 1);
    all.push(earlier);
    all.extend(chips);
    view! { <div>{ all }</div> }.into_any()
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
        // v0.1.5 writes one `hook.<window>.chain` marker per hook run
        // (a verbose per-step JSON trace). It is routine, so it stays
        // out of the strip; `.error` / `.unknown_fields` are kept
        // because they mean a hook is failing or still speaking the
        // legacy envelope.
        .filter(|k| !k.ends_with(".chain"))
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
    // v0.5.45: which model and thinking effort this session starts with.
    // Empty model = follow the config's active entry.
    let model = RwSignal::new(String::new());
    let effort = RwSignal::new(String::new());
    let entries = RwSignal::new(Vec::<crate::model::ModelEntry>::new());

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
            model.set(String::new());
            effort.set(String::new());
            // The entries feed both the model picker and (through their
            // model_id) the level list the effort picker offers.
            spawn_local(async move {
                if let Ok(v) = api::load_model().await {
                    entries.set(v.entries);
                }
            });
            let def = if def.is_empty() { None } else { Some(def) };
            browse_to(def);
        }
    });

    // Defer the close to a macrotask: a microtask can still land
    // mid-dispatch (this Chromium build drains microtasks between the
    // target handler and the bubble listeners), which would unmount
    // the dialog while its own listeners are still on the propagation
    // path and throw "closure invoked after being dropped".
    let close = move || {
        let open = open;
        after_dispatch(move || open.set(None));
    };

    let create = move || {
        let n = name.get().trim().to_string();
        if n.is_empty() {
            create_err.set(Some("name is empty".to_string()));
            return;
        }
        let c = cwd.get();
        let cwd_opt = if c.trim().is_empty() { None } else { Some(c.trim().to_string()) };
        // v0.5.45: the model and effort picked here become the session's
        // markers; empty = follow the config's active entry.
        let m = model.get();
        let model_opt = if m.trim().is_empty() { None } else { Some(m.trim().to_string()) };
        let e = effort.get();
        let effort_opt = if e.trim().is_empty() { None } else { Some(e.trim().to_string()) };
        busy.set(true);
        create_err.set(None);
        spawn_local(async move {
            match api::create_session(&n, cwd_opt.as_deref(), model_opt.as_deref(), effort_opt.as_deref())
                .await
            {
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

                    <label class="ns-label" for="ns-name">{ "Name" }</label>
                    <input id="ns-name" class="ns-input" placeholder="session name" bind:value=name />

                    <label class="ns-label" for="ns-cwd">{ "Working directory" }</label>
                    <input id="ns-cwd" class="ns-input" placeholder="/path/to/project" bind:value=cwd />

                    // v0.5.45: model + thinking effort for THIS session.
                    // The model panel deliberately has no global default.
                    <label class="ns-label">{ "model" }</label>
                    <div class="ms-seg ns-seg">
                        <button
                            class:on=move || model.get().is_empty()
                            on:click=move |_| model.set(String::new())
                        >{ "follow config" }</button>
                        { move || {
                            entries
                                .get()
                                .into_iter()
                                .map(|e| {
                                    let name = e.name.clone();
                                    let n2 = name.clone();
                                    view! {
                                        <button
                                            class:on=move || model.get() == n2
                                            on:click=move |_| {
                                                let name = name.clone();
                                                model.set(name);
                                                effort.set(String::new());
                                            }
                                        >{ e.name.clone() }</button>
                                    }
                                })
                                .collect_view()
                        } }
                    </div>
                    <label class="ns-label">{ "thinking effort" }</label>
                    <div class="ms-seg ns-seg">
                        <button
                            class:on=move || effort.get().is_empty()
                            on:click=move |_| effort.set(String::new())
                        >{ "inherit" }</button>
                        { move || {
                            let id = entries
                                .get()
                                .into_iter()
                                .find(|e| e.name == model.get())
                                .and_then(|e| e.model_id)
                                .unwrap_or_default();
                            crate::ms::effort_choices(&id)
                                .into_iter()
                                .map(|v| {
                                    let vv = v.to_string();
                                    view! {
                                        <button
                                            class:on=move || effort.get() == vv
                                            on:click=move |_| {
                                                let v = v.to_string();
                                                effort.set(v);
                                            }
                                        >{ v }</button>
                                    }
                                })
                                .collect_view()
                        } }
                    </div>

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

/// Delete-confirmation sub-window (in-app replacement for the native
/// `window.confirm`): Some(name) opens the dialog for that session.
/// Models the NewSessionDialog markup; the Delete action hands off to
/// `delete_session` (which awaits the API call and surfaces errors).
#[component]
pub fn DeleteConfirmDialog(state: AppState) -> impl IntoView {
    let open = state.confirm_delete;

    // Same as NewSessionDialog: unmount on a macrotask, after the
    // click has fully finished dispatching.
    let close = move || {
        let open = open;
        after_dispatch(move || open.set(None));
    };

    let confirm = move |_| {
        let name = open.get().unwrap_or_default();
        let open = open;
        after_dispatch(move || {
            open.set(None);
            if !name.is_empty() {
                delete_session(state, &name);
            }
        });
    };

    view! {
        <Show when=move || open.get().is_some() fallback=|| ()>
            <div id="dc-backdrop" on:click=move |_| close()>
                <div id="dc-dialog" on:click=move |e: MouseEvent| e.stop_propagation()>
                    <div class="dc-title">{ "Delete Session" }</div>
                    { move || open.get().map(|name| view! {
                        <div class="dc-body">
                            { format!("Delete session \"{name}\" and all its data? This cannot be undone.") }
                        </div>
                    }) }
                    <div class="dc-actions">
                        <button class="ns-btn ns-cancel" on:click=move |_| close()>{ "Cancel" }</button>
                        <button class="ns-btn ns-danger" on:click=confirm>{ "Delete" }</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

// ── input module (port of doSend / key) ───────────────────────────

/// v0.5.6: auto-fit #msg-input to its content. The height grows with
/// the text but caps at one third of the main panel (#main) so a wall
/// of text can't swallow the UI; past the cap the textarea scrolls
/// internally (#msg-input already has `overflow-y: auto`). One
/// reflow per call (height:auto → measure → set px). Called on every
/// input event, after programmatic clears, and on window resize /
/// keyboard lift (the pile engine's resize + vv handlers).
///
/// v0.5.48, three fixes around the same root: `height:auto` on a
/// GROWN textarea momentarily shrinks the input module, which GROWS
/// #transcript for one layout pass, and the browser then clamps
/// #transcript.scrollTop to that transient max — the "bottom as of the
/// default input height" position. Nothing put it back, so it read as
/// an unexplained upward move: `release:unexplained`, the follower
/// released, and with the 500 ms `on_vv_event` poll re-running this on
/// an idle input, the viewport was yanked every half second (and the
/// grown input box hid the newest card's bottom).
///   (a) a call whose inputs (value + cap) are unchanged is a no-op, so
///       the poll can poll without touching the layout;
///   (b) the measure dance can never move the transcript — the
///       scroll_top is restored if the transient clamped it;
///   (c) a real height change notifies the engine, which re-parks the
///       live edge while following (and stays put while reading).
pub fn size_msg_input() {
    thread_local! {
        /// (value, cap px) of the last measurement: the no-op guard's
        /// inputs. Only `size_msg_input` ever writes #msg-input's
        /// inline height, so an unchanged pair means the applied height
        /// is still the one we measured.
        static LAST_FIT: std::cell::RefCell<Option<(String, f64)>> =
            const { std::cell::RefCell::new(None) };
    }
    let Some(w) = web_sys::window() else { return };
    let Some(doc) = w.document() else { return };
    // HtmlTextAreaElement (for `.value()`), which derefs to HtmlElement
    // (style / offset_height / scroll_height).
    let Some(ta) = doc
        .get_element_by_id("msg-input")
        .and_then(|e| e.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
    else {
        return;
    };
    let cap = doc
        .get_element_by_id("main")
        .map(|m| m.unchecked_into::<web_sys::HtmlElement>())
        .map(|m| m.client_height() as f64 * (1.0 / 3.0))
        .unwrap_or(120.0);
    // (a) Nothing that determines the height changed: leave the layout —
    // and with it the transcript's scroll_top — completely alone.
    let value = ta.value();
    let unchanged = LAST_FIT.with(|c| {
        c.borrow()
            .as_ref()
            .map(|(v0, cap0)| v0 == &value && (cap0 - cap).abs() < 0.5)
            .unwrap_or(false)
    });
    if unchanged {
        return;
    }
    // (b) Save the transcript's scroll_top across the dance below.
    // `el` is pinned to HtmlElement: on the textarea handle `style()`
    // would resolve to Leptos' ElementExt::style (a builder, not the
    // CSSStyleDeclaration accessor).
    let el: &web_sys::HtmlElement = &ta;
    // HtmlElement (not Element): the restore below needs `style()`.
    let tr = doc
        .get_element_by_id("transcript")
        .map(|t| t.unchecked_into::<web_sys::HtmlElement>());
    let saved_top = tr.as_ref().map(|t| t.scroll_top());
    let h_before = el.offset_height() as f64;
    let _ = el.style().set_property("height", "auto");
    let h = (el.scroll_height() as f64).clamp(40.0, cap.max(40.0));
    let _ = el.style().set_property("height", &format!("{h:.0}px"));
    if let (Some(t), Some(top)) = (&tr, saved_top) {
        if t.scroll_top() != top {
            // v0.5.49: #transcript carries `scroll-behavior: smooth`, so
            // a bare programmatic write here is a ~300 ms ANIMATED
            // scroll: the engine samples a moving viewport with no input
            // evidence for the whole animation (and the per-frame pin
            // fights it). Every other transcript write in the engine
            // opts out of smooth first — this restore must too. The
            // previous inline value is put back.
            let prev_css = t.style().get_property_value("scroll-behavior").ok();
            let _ = t.style().set_property("scroll-behavior", "auto");
            let _ = t.set_scroll_top(top);
            if let Some(p) = prev_css {
                let _ = t.style().set_property("scroll-behavior", &p);
            }
        }
    }
    LAST_FIT.with(|c| *c.borrow_mut() = Some((value, cap)));
    // (c) The input module's height really moved: #transcript's bottom
    // edge moved with it, so a follower must re-park at the live edge.
    let h_after = el.offset_height() as f64;
    if (h_after - h_before).abs() > 0.5 {
        crate::pile::on_input_height_changed();
    }
}

/// One row of the queue-mode popup (steer / follow). A
/// raised-look option button: hover tucks it into the panel
/// (groove), the active one gets an accent tick.
fn qsel_opt(
    value: String,
    label: &'static str,
    qsel_value: RwSignal<String>,
    qsel_open: RwSignal<bool>,
) -> impl IntoView {
    let click_value = value.clone();
    let selected = move || qsel_value.get() == value;
    view! {
        <button
            class="qsel-opt"
            aria-selected=selected
            on:click=move |_| {
                qsel_value.set(click_value.clone());
                qsel_open.set(false);
            }
        >
            <span class="qsel-tick">{"✓"}</span>
            { label }
        </button>
    }
}

/// v0.5.44: fixed 16×16 play/stop icons for #btn-send. Replaces the
/// U+25B6/U+25A0 text glyphs, whose font-dependent metrics left the
/// mark off-centre in the button. Both shapes are drawn centred on the
/// 24-unit grid and sized/filled by CSS (`#btn-send svg` / `.fl`), so
/// the mark sits exactly in the button's centre.
fn send_stop_icon(running: bool) -> AnyView {
    if running {
        view! {
            <svg class="send-ic" viewBox="0 0 24 24" aria-hidden="true">
                <path class="fl" d="M6 6h12v12H6z" />
            </svg>
        }
        .into_any()
    } else {
        view! {
            <svg class="send-ic" viewBox="0 0 24 24" aria-hidden="true">
                <path class="fl" d="M8 5v14l11-7z" />
            </svg>
        }
        .into_any()
    }
}

#[component]
pub fn InputModule(state: AppState) -> impl IntoView {
    let msg = RwSignal::new(String::new());
    // v0.5.6: the send button doubles as the loop start/stop control
    // (green triangle = send + start; red square = stop).
    let loop_running = state.loop_running;
    // Queue-mode picker (steer/follow). A custom popup instead
    // of the native <select>: the OS-rendered dropdown list can't be
    // themed, so the panel is our own DOM, styled with the same rice
    // neumorphism as the rest of the chrome. `qsel_value` is "steer"
    // by default; the send paths read it instead of scraping the DOM.
    let qsel_open = RwSignal::new(false);
    // Default queue is `steer` (the "next step" injection). Only
    // `follow` is distinct (runs after the loop stops). The old
    // `direct` option was just the no-field steer path, now dropped.
    let qsel_value = RwSignal::new("steer".to_string());


    view! {
        <div id="input-module">
            <StatusStrip state=state.clone() />
            <div id="input-area">
                <div
                    id="queue-select"
                    class=move || if qsel_open.get() { "open".to_string() } else { String::new() }
                >
                    <button
                        class="qsel-btn"
                        on:click=move |_| qsel_open.update(|o| *o = !*o)
                    >
                        { qsel_value.clone() }
                        <span class="qsel-chev">{"▾"}</span>
                    </button>
                    <Show
                        when=move || qsel_open.get()
                        fallback=move || ()
                    >
                        {
                            let qv = qsel_value.clone();
                            let qo = qsel_open.clone();
                            view! {
                                <div
                                    class="qsel-backdrop"
                                    on:click=move |_| qo.set(false)
                                />
                                <div class="qsel-panel">
                                    { qsel_opt("steer".to_string(), "steer", qv.clone(), qo.clone()) }
                                    { qsel_opt("follow".to_string(), "follow", qv, qo) }
                                </div>
                            }
                        }
                    </Show>
                </div>
                <textarea
                    id="msg-input"
                    placeholder="Send a message... (Ctrl+Enter to send)"
                    rows="1"
                    bind:value=msg
                    on:input=move |_| { size_msg_input(); }
                    on:keydown=move |e: KeyboardEvent| {
                        if e.key() == "Enter" && (e.ctrl_key() || e.meta_key()) {
                            e.prevent_default();
                            let q = qsel_value.get().clone();
                            let text = msg.get();
                            msg.set(String::new());
                            size_msg_input();
                            let s = state;
                            spawn_local(async move {
                                do_send(s, text, q).await;
                            });
                        }
                    }
                />
                <button
                    id="btn-send"
                    class=move || {
                        // v0.5.6: the send button doubles as the loop
                        // control (the sidebar start/stop buttons were
                        // folded in). Green triangle = idle/send, red
                        // square = loop running (click stops it).
                        if loop_running.get() { "stop".to_string() } else { String::new() }
                    }
                    title=move || {
                        if loop_running.get() {
                            "Stop loop".to_string()
                        } else {
                            "Send (starts the loop)".to_string()
                        }
                    }
                    on:click=move |_| {
                        if loop_running.get() {
                            // Red square: the loop is running, so this
                            // click stops it (the old #btn-stop).
                            stop_loop(state);
                            return;
                        }
                        // Green triangle: send the message; do_send
                        // ensures the loop is running (the old
                        // #btn-start). An empty message does nothing.
                        let q = qsel_value.get().clone();
                        let text = msg.get();
                        msg.set(String::new());
                        size_msg_input();
                        let s = state;
                        spawn_local(async move {
                            do_send(s, text, q).await;
                        });
                    }
                >
                    { move || send_stop_icon(loop_running.get()) }
                </button>
            </div>
        </div>
    }
}

// ── M8/M11: right tool panel — Files tree + browser-style multi-tab ──

/// M8: true when `name` ends with an image extension (the preview pane
/// renders an <img> via the raw endpoint instead of a text read, which
/// returns 415 for binary files).
fn is_image_ext(name: &str) -> bool {
    let ext = name.rsplit('.').next().map(|s| s.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some("png")
            | Some("jpg")
            | Some("jpeg")
            | Some("gif")
            | Some("webp")
            | Some("svg")
            | Some("ico")
            | Some("bmp")
    )
}

/// M8/M11: 14×14 folder icon for the Files home tab (inline SVG so the
/// glyph is stable across fonts; strokes inherit `currentColor`).
fn tab_icon_files() -> impl IntoView {
    view! {
        <svg class="rp-tab-ic" viewBox="0 0 16 16" aria_hidden="true">
            <path class="ln" d="M2 4a1 1 0 0 1 1-1h4l1.5 2H13a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1z" />
        </svg>
    }
}

/// M11: 14×14 document icon for a File tab.
fn tab_icon_file() -> impl IntoView {
    view! {
        <svg class="rp-tab-ic" viewBox="0 0 16 16" aria_hidden="true">
            <path class="ln" d="M4 2h5l3 3v9H4z" />
            <path class="ln" d="M9 2v3h3" />
        </svg>
    }
}

/// M8/M11: 14×14 terminal-prompt icon for terminal tabs.
fn tab_icon_term() -> impl IntoView {
    view! {
        <svg class="rp-tab-ic" viewBox="0 0 16 16" aria_hidden="true">
            <rect class="ln" x="1.5" y="3" width="13" height="10" rx="2" />
            <path class="ln" d="M4.5 6.5 7 9l-2.5 2.5" />
            <path class="ln" d="M9.5 11.5H12" />
        </svg>
    }
}

/// M8/M11: extension label for the preview name chip ("PNG", "RS", …),
/// falling back to "FILE" for extension-less names.
fn ext_label(name: &str) -> String {
    name.rsplit('.')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| "FILE".to_string())
}

/// M11: human byte size for the preview size hint ("4.2 KB").
fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    }
}

/// M11: hard cap on concurrently open terminal tabs. The server enforces
/// the same cap on the per-connection pty pool.
const MAX_TERM_TABS: u32 = 4;

/// M11: a File tab already open for this workdir-relative path? (tree
/// "selected" mark + open-file reuse).
fn has_file_tab(state: AppState, rel: &str) -> bool {
    state
        .rp_tabs
        .get()
        .iter()
        .any(|t| t.kind == RpTabKind::File && t.path == rel)
}

/// M8: lazily fetch one directory's listing and cache it in
/// `rp_children`. A stale response (the user switched session in the
/// meantime) must not pollute the current view, so the write is guarded
/// by a session check. Failures surface in the Files home tab's error
/// slot (`rp_err_map[0]`).
fn ensure_dir_loaded(state: AppState, rel: &str) {
    let sess = match state.active_session.get() {
        Some(s) => s,
        None => return,
    };
    let rel_owned = rel.to_string();
    spawn_local(async move {
        match api::list_dir(&sess, &rel_owned).await {
            Ok(dl) => {
                if state.active_session.get() == Some(sess.clone()) {
                    state
                        .rp_children
                        .update(|m| { m.insert(rel_owned.clone(), dl.entries); });
                }
                if rel_owned.is_empty() {
                    // A successful root re-list clears the home-tab error.
                    state.rp_err_map.update(|m| {
                        m.remove(&0u32);
                    });
                }
            }
            Err(e) => {
                state
                    .rp_err_map
                    .update(|m| { m.insert(0, format!("cannot list {rel_owned}: {e}")); });
            }
        }
    });
}

/// M8: toggle a directory expanded/collapsed; fetch on first expand.
fn toggle_dir(state: AppState, rel: &str) {
    let expanded = state.rp_expanded;
    let opening = !expanded.get().contains(rel);
    expanded.update(|s| {
        if opening {
            s.insert(rel.to_string());
        } else {
            s.remove(rel);
        }
    });
    if opening && !state.rp_children.get().contains_key(rel) {
        ensure_dir_loaded(state, rel);
    }
}

/// M11: open a file in the panel. If a tab already has it, just focus
/// that tab; otherwise allocate a fresh tab id, push it, focus it, and
/// fetch the preview into `rp_preview_map[id]` (read errors land in
/// `rp_err_map[id]`, images are an exception — their preview pane renders
/// the raw <img> instead).
fn open_file(state: AppState, rel: &str) {
    let rel_owned = rel.to_string();
    if let Some(id) = state
        .rp_tabs
        .get()
        .iter()
        .find(|t| t.kind == RpTabKind::File && t.path == rel_owned)
        .map(|t| t.id)
    {
        state.rp_active.set(id);
        return;
    }
    let sess = match state.active_session.get() {
        Some(s) => s,
        None => return,
    };
    let id = state.rp_next_id.get();
    state.rp_next_id.update(|n| *n += 1);
    let label = rel_owned
        .rsplit('/')
        .next()
        .unwrap_or(rel_owned.as_str())
        .to_string();
    state.rp_tabs.update(|tabs| {
        tabs.push(RpTab {
            id,
            kind: RpTabKind::File,
            path: rel_owned.clone(),
            label,
        });
    });
    state.rp_active.set(id);
    let rp_preview_map = state.rp_preview_map;
    let rp_err_map = state.rp_err_map;
    spawn_local(async move {
        match api::read_file(&sess, &rel_owned).await {
            Ok(pv) => {
                if state.active_session.get() == Some(sess.clone()) {
                    rp_preview_map.update(|m| { m.insert(id, Some(pv)); });
                }
            }
            Err(e) => {
                // Images fail the text read (binary → 415); the preview
                // pane renders the raw <img> for image extensions, so
                // only non-images surface an error here.
                if !is_image_ext(&rel_owned) {
                    rp_err_map.update(|m| { m.insert(id, e); });
                }
            }
        }
    });
}

/// M11: open a new terminal tab (up to MAX_TERM_TABS; the server enforces
/// the same cap on its pty pool). The tab id doubles as the server pty
/// id, so each tab owns exactly one shell.
fn new_terminal(state: AppState) {
    if state.active_session.get().is_none() {
        return;
    }
    let term_count = state
        .rp_tabs
        .get()
        .iter()
        .filter(|t| t.kind == RpTabKind::Term)
        .count() as u32;
    if term_count >= MAX_TERM_TABS {
        // Surface the limit in the panel's error slot (the Files tab),
        // auto-cleared after a few seconds.
        state.rp_err_map.update(|m| {
            m.insert(0, "terminal limit reached (4 open)".to_string());
        });
        let errmap = state.rp_err_map;
        spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(4000).await;
            errmap.update(|m| {
                if m.get(&0u32).is_some_and(|e| e.starts_with("terminal limit")) {
                    m.remove(&0u32);
                }
            });
        });
        return;
    }
    let id = state.rp_next_id.get();
    state.rp_next_id.update(|n| *n += 1);
    let seq = state.rp_term_seq.get();
    state.rp_term_seq.update(|n| *n += 1);
    let label = format!("Term {seq}");
    state.rp_tabs.update(|tabs| {
        tabs.push(RpTab {
            id,
            kind: RpTabKind::Term,
            path: String::new(),
            label,
        });
    });
    state
        .term_state
        .update(|m| {
            m.entry(id).or_default();
        });
    state.rp_active.set(id);
}

/// M11: close one tab. Terminal tabs close their pty on the server
/// (`term_close{id}` — the connection stays open, the other tabs'
/// shells keep running). File tabs just drop their cached preview/error.
/// The Files home tab (id 0) is permanent. After closing the active tab,
/// focus moves to the previous tab (or the home tab).
fn close_tab(state: AppState, id: u32) {
    if id == 0 {
        return; // the Files home tab is permanent
    }
    let kind = match state.rp_tabs.get().iter().find(|t| t.id == id) {
        Some(t) => t.kind,
        None => return,
    };
    if kind == RpTabKind::Term {
        ws::term_close(id);
        ws::clear_term_writer(id);
        state.term_state.update(|m| { m.remove(&id); });
    }
    state.rp_preview_map.update(|m| { m.remove(&id); });
    state.rp_err_map.update(|m| { m.remove(&id); });
    let tabs = state.rp_tabs.get();
    let idx = tabs.iter().position(|t| t.id == id).unwrap_or(0);
    let prev = tabs.iter().take(idx).last().map(|t| t.id).unwrap_or(0);
    state.rp_tabs.update(|ts| ts.retain(|t| t.id != id));
    if state.rp_active.get() == id {
        state.rp_active.set(prev);
    }
}

/// M8/M11: one tree node. A directory shows a caret + its (lazily
/// fetched) children when expanded; a file opens its own tab. The
/// "selected" mark = a File tab is open for this path. `parent_rel` is
/// the workdir-relative path of the containing directory ("" = root).
fn tree_node(state: AppState, e: DirEntry, parent_rel: String, depth: u32) -> impl IntoView {
    let name = e.name.clone();
    let is_dir = e.is_dir;
    let expanded = state.rp_expanded;
    let rel = if parent_rel.is_empty() {
        name.clone()
    } else {
        format!("{parent_rel}/{name}")
    };
    // One owned clone per `move` handler (M7 idiom): each view! closure
    // captures its own copy, since a single String can't be moved into
    // two closures.
    let rel_cls = rel.clone();
    let rel_caret = rel.clone();
    let rel_name = rel.clone();
    let rel_when = rel.clone();
    view! {
        <div
            class=move || {
                let mut c = String::from("rp-node");
                if is_dir && expanded.get().contains(&rel_cls) {
                    c.push_str(" open");
                }
                // M11: "selected" = a File tab is open for this path.
                if has_file_tab(state, &rel_cls) {
                    c.push_str(" sel");
                }
                c
            }
        >
            // M12: head row (caret + name) is a horizontal flex row; the
            // expanded children render in a SIBLING block below it
            // (.rp-node-kids) so the tree grows downward. The old layout
            // made .rp-node itself a flex row with the children as inline
            // flex items, so expanding a directory pushed its subfolders to
            // the RIGHT of the name and clipped wide/deep trees in the
            // fixed 420px panel.
            <div class="rp-node-head">
                <button
                    class=move || if is_dir { "rp-caret" } else { "rp-caret leaf" }
                    title=rel_caret.clone()
                    on:click=move |_| {
                        if is_dir {
                            toggle_dir(state, &rel_caret);
                        }
                    }
                >
                    { if is_dir { "\u{25b8}" } else { "\u{b7}" } }
                </button>
                <span
                    class="rp-node-name"
                    title=rel_name.clone()
                    on:click=move |_| {
                        if is_dir {
                            toggle_dir(state, &rel_name);
                        } else {
                            open_file(state, &rel_name);
                        }
                    }
                >
                    { name.clone() }
                </span>
            </div>
            <Show
                when=move || is_dir && expanded.get().contains(&rel_when)
                fallback=|| ()
            >
                <div class="rp-node-kids">
                    { tree_level(state, rel.clone(), depth + 1) }
                </div>
            </Show>
        </div>
    }
}

/// M8: the children of one directory level. Entries come from the
/// lazily-filled `rp_children` cache ("" = the workdir root), so the
/// level renders nothing until its listing arrives.
fn tree_level(state: AppState, rel: String, depth: u32) -> AnyView {
    // One owned clone per `move` closure (the Memo's read closure and
    // the For's children closure), since a single String can't be moved
    // into both.
    let rel_memo = rel.clone();
    let rel_child = rel.clone();
    let entries = Memo::new(move |_prev: Option<&Vec<DirEntry>>| {
        let map = state.rp_children.get();
        map.get(&rel_memo).cloned().unwrap_or_default()
    });
    view! {
        <For
            each=move || entries.get()
            key=|e: &DirEntry| e.name.clone()
            children=move |e| tree_node(state, e, rel_child.clone(), depth)
        />
    }
    .into_any()
}

/// M11: the Files home tab (id 0) — the working directory's lazy tree,
/// a small cwd hint header, and the panel's error slot. File previews
/// live in their own tabs, so the home tab is tree-only and fills the
/// whole pane.
fn files_tab(state: AppState) -> impl IntoView {
    // Fetch the workdir root listing on mount / session change. The
    // guard makes it a no-op once the root is cached, so the effect
    // that re-runs when the cache fills in does not loop.
    Effect::new(move || {
        let sess = state.active_session.get();
        if sess.is_none() {
            return;
        }
        let cached = state.rp_children.get().contains_key("");
        if !cached {
            ensure_dir_loaded(state, "");
        }
    });

    let active = state.active_session;
    let sessions = state.sessions;
    let rp_err_map = state.rp_err_map;

    view! {
        <div class="rp-files">
            <div class="rp-tree-meta">
                <span class="rp-tree-cwd">
                    { move || {
                        let cwd = active
                            .get()
                            .as_ref()
                            .and_then(|n| {
                                sessions
                                    .get()
                                    .iter()
                                    .find(|s| s.name == *n)
                                    .and_then(|s| s.cwd.clone())
                            })
                            .unwrap_or_default();
                        cwd.rsplit('/')
                            .next()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("workdir")
                            .to_string()
                    } }
                </span>
            </div>
            <div class="rp-tree">
                { move || {
                    if active.get().is_none() {
                        view! { <div class="rp-empty">{ "select a session" }</div> }.into_any()
                    } else if state.rp_children.get().contains_key("") {
                        tree_level(state, String::new(), 0)
                    } else {
                        view! { <div class="rp-empty rp-loading">{ "loading…" }</div> }.into_any()
                    }
                } }
            </div>
            { move || {
                match rp_err_map.get().get(&0u32).cloned() {
                    Some(e) => view! { <div class="rp-err">{ e }</div> }.into_any(),
                    None => view! { <div class="rp-empty rp-loading">{ "" }</div> }.into_any(),
                }
            } }
        </div>
    }
}

/// M11: the preview pane of one File tab — a sunken card with a name
/// pill, extension chip, size hint, and the content (pre/code for
/// text, an <img> for image extensions, an error card for read
/// failures).
fn file_tab_view(state: AppState, tab: RpTab) -> impl IntoView {
    let id = tab.id;
    let rel = tab.path;
    let label = tab.label;
    let rp_preview_map = state.rp_preview_map;
    let rp_err_map = state.rp_err_map;
    let active = state.active_session;

    view! {
        <div class="rp-preview">
            <div class="rp-preview-bar">
                <span class="rp-preview-name" title={ label.clone() }>
                    { label.clone() }
                </span>
                <span class="rp-ext">{ ext_label(&rel) }</span>
                <span class="rp-trunc">
                    { move || {
                        rp_preview_map
                            .get()
                            .get(&id)
                            .and_then(|p| p.as_ref())
                            .is_some_and(|p| p.truncated)
                            .then(|| "… truncated".to_string())
                            .unwrap_or_default()
                    } }
                </span>
                <span class="rp-size">
                    { move || {
                        rp_preview_map
                            .get()
                            .get(&id)
                            .and_then(|p| p.as_ref())
                            .map(|p| fmt_bytes(p.size))
                            .unwrap_or_default()
                    } }
                </span>
            </div>
            { move || {
                let pv = rp_preview_map.get().get(&id).cloned().flatten();
                let err = rp_err_map.get().get(&id).cloned();
                if is_image_ext(&rel) {
                    let act = active;
                    let src_s = rel.clone();
                    let alt_s = rel.clone();
                    view! {
                        <div class="rp-img-wrap">
                            <img
                                class="rp-img"
                                src=move || {
                                    let sess = act.get().unwrap_or_default();
                                    api::raw_url(&sess, &src_s)
                                }
                                alt=move || alt_s.clone()
                            />
                        </div>
                    }
                    .into_any()
                } else if let Some(p) = pv {
                    view! {
                        <pre class="rp-code"><code>{ p.content }</code></pre>
                    }
                    .into_any()
                } else if let Some(e) = err {
                    view! {
                        <div class="rp-err">{ e }</div>
                    }
                    .into_any()
                } else {
                    view! {
                        <div class="rp-empty rp-loading">{ "loading…" }</div>
                    }
                    .into_any()
                }
            } }
        </div>
    }
}

/// M11: the body pane of one right-panel tab. All open tabs stay mounted
/// (terminal shells keep running while their tab is hidden); CSS makes
/// only the active pane visible — via `visibility` (not `display`) so
/// hidden panes keep their size and xterm's scrollback + pty dims
/// survive tab switches.
fn tab_pane(state: AppState, tab: RpTab) -> AnyView {
    let t_id = tab.id;
    let content: AnyView = match tab.kind {
        RpTabKind::Files => files_tab(state).into_any(),
        RpTabKind::File => file_tab_view(state, tab.clone()).into_any(),
        RpTabKind::Term => crate::terminal::TerminalView(
            crate::terminal::TerminalViewProps {
                state,
                term_id: t_id,
            },
        )
        .into_any(),
    };
    view! {
        <div
            class=move || {
                let mut c = String::from("rp-pane");
                if state.rp_active.get() == t_id {
                    c.push_str(" active");
                }
                c
            }
        >
            { content }
        </div>
    }
    .into_any()
}

/// M8/M11: the right tool panel — a fixed 420 px column in #app
/// (sibling of #main, so #main shrinks to make room). Browser-style
/// multi-tab: a persistent "Files" home tab (id 0, not closable) owns
/// the directory tree; every opened file or terminal becomes its own
/// closable tab; the "+" button spawns a new terminal (max 4, mirroring
/// the server's pty cap). Hidden in the "full" dispatch layout (that
/// view owns the whole window) and when the context-bar toggle is off.
#[component]
pub fn RightPanel(state: AppState) -> impl IntoView {
    let tabs = state.rp_tabs;
    let active_id = state.rp_active;
    let active_session = state.active_session;

    view! {
        <aside id="right-panel">
            <div class="rp-tabbar">
                <div class="rp-tabs">
                    <For
                        each=move || tabs.get()
                        key=|t: &RpTab| t.id.to_string()
                        children=move |tab| {
                            let t_id = tab.id;
                            let t_kind = tab.kind;
                            let t_label = tab.label.clone();
                            let closable = tab.id != 0;
                            view! {
                                <div
                                    class=move || {
                                        let mut c = String::from("rp-tab-item");
                                        if active_id.get() == t_id {
                                            c.push_str(" active");
                                        }
                                        c
                                    }
                                >
                                    <button
                                        class="rp-tab-main"
                                        title=t_label.clone()
                                        on:click=move |_| {
                                            state.rp_active.set(t_id);
                                        }
                                    >
                                        {
                                            let icon: AnyView = match t_kind {
                                                RpTabKind::Files => tab_icon_files().into_any(),
                                                RpTabKind::File => tab_icon_file().into_any(),
                                                RpTabKind::Term => tab_icon_term().into_any(),
                                            };
                                            icon
                                        }
                                        <span class="rp-tab-label">{ t_label.clone() }</span>
                                    </button>
                                    {
                                        move || {
                                            if closable {
                                                Some(view! {
                                                    <button
                                                        class="rp-tab-x"
                                                        title="close tab"
                                                        on:click=move |_| close_tab(state, t_id)
                                                    >
                                                        { "\u{00d7}" }
                                                    </button>
                                                })
                                            } else {
                                                None
                                            }
                                        }
                                    }
                                </div>
                            }
                        }
                    />
                </div>
                <button
                    class="rp-tab-new"
                    title="new terminal (max 4 open)"
                    disabled=move || active_session.get().is_none()
                    on:click=move |_| new_terminal(state)
                >
                    { "+" }
                </button>
            </div>
            <div class="rp-body">
                <For
                    each=move || tabs.get()
                    key=|t: &RpTab| t.id.to_string()
                    children=move |tab| tab_pane(state, tab)
                />
            </div>
        </aside>
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
