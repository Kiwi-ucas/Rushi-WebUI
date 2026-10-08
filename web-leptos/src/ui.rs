//! Sidebar / goal / context bar / input / welcome (port of the JS DOM code).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use web_sys::{DragEvent, KeyboardEvent, MouseEvent};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::api;
use crate::plugins::PluginDef;
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

pub(crate) fn set_layout_mode(mode: &str) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item("rushi-layout", mode);
    }
}

// ── project labels (v0.5.57) ──────────────────────────────────────
// The session groups are keyed by the session's **working path** (its `cwd`
// marker, `model::dispatch_groups`). The group head shows that path's
// basename; the user may rename that *label* — a display-only alias kept in
// localStorage next to every other UI preference (`rushi-project-labels`,
// a `{ "<full path>": "<label>" }` object). The working path itself is never
// touched: no server call, no write into the session directory, and the full
// path stays the tooltip.

/// The persisted path → display-label map. Tolerant: a missing key, a broken
/// JSON blob or a non-object all read as "no aliases".
pub fn read_project_labels() -> std::collections::HashMap<String, String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("rushi-project-labels").ok())
        .flatten()
        .and_then(|v| serde_json::from_str::<std::collections::HashMap<String, String>>(&v).ok())
        .unwrap_or_default()
}

pub fn persist_project_labels(labels: &std::collections::HashMap<String, String>) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item(
            "rushi-project-labels",
            &serde_json::to_string(labels).unwrap_or_else(|_| "{}".into()),
        );
    }
}

// ── per-session composer drafts (v0.5.58) ───────────────────────────
// The main input box is bound to the ACTIVE session (model::AppState
// `draft` / `drafts`). On a session switch the outgoing session's unsent
// text is stashed here and the incoming session's own draft restored, so
// the box never carries one session's words into another and a send can't
// land in the wrong session. The map is persisted to localStorage so it
// survives a full page reload.

/// The persisted session-name → unsent-text map. Tolerant: a missing key,
/// a broken JSON blob, or a non-object all read as "no drafts".
pub fn read_drafts() -> std::collections::HashMap<String, String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item("rushi-drafts").ok())
        .flatten()
        .and_then(|v| {
            serde_json::from_str::<std::collections::HashMap<String, String>>(&v).ok()
        })
        .unwrap_or_default()
}

pub fn persist_drafts(drafts: &std::collections::HashMap<String, String>) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item(
            "rushi-drafts",
            &serde_json::to_string(drafts).unwrap_or_else(|_| "{}".into()),
        );
    }
}

/// The label a group head shows for `group_key` (a working path): the user's
/// alias when one is set, else the path's basename (`dispatch_group_label`).
pub fn group_label(state: AppState, group_key: &str) -> String {
    let alias = state
        .project_labels
        .get()
        .get(group_key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    alias.unwrap_or_else(|| crate::model::dispatch_group_label(group_key))
}

/// Set (or clear, with an empty label) the display alias of one working path
/// and persist the map. Clearing restores the basename.
pub fn set_group_label(state: AppState, group_key: &str, label: &str) {
    let key = group_key.to_string();
    let label = label.trim().to_string();
    state.project_labels.update(|m| {
        if label.is_empty() {
            m.remove(&key);
        } else {
            m.insert(key.clone(), label.clone());
        }
    });
    persist_project_labels(&state.project_labels.get());
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

pub(crate) fn set_theme_mode_stored(mode: &str) {
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
pub(crate) fn theme_icon(mode: &str) -> AnyView {
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
/// with, plus a popup to change it. The chip is disabled while the
/// session's loop runs: the choice applies at the next launch, so
/// changing it mid-run would only mislead (and, worse, make the chip
/// disagree with what is running).
///
/// v0.5.76: the popup speaks the SIDEBAR's language (`.smc-*` — flat
/// 11px/700 rows on the `#sess-menu` panel skin) instead of borrowing
/// the message box's steer selector (`.qsel-*`, 13.3px raised pills);
/// and it grew a "thinking effort" row (`ms::effort_choices`, written to
/// the session's `.effort` marker — the same field the new-session
/// dialog sets). Both choices apply at the next launch.
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
    let model_ids = state.model_ids;
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
    // v0.5.76: the session's pending effort ("" = follow the entry's own
    // `reasoning_effort`). Read back from the session list like `current`,
    // so a write followed by the list reload updates the pills.
    let current_effort = Signal::derive(move || {
        let n = session_name.get();
        sessions
            .get()
            .into_iter()
            .find(|s| s.name == n)
            .and_then(|s| s.effort)
            .unwrap_or_default()
    });
    // The levels this model family accepts (family inferred from the
    // provider model_id; unknown/empty => every level the kernel knows).
    let levels = Signal::derive(move || {
        let id = model_ids
            .get()
            .get(&current.get())
            .cloned()
            .unwrap_or_default();
        crate::ms::effort_choices(&id)
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<String>>()
    });

    view! {
        <div class="sess-model" class:open=move || open.get()>
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
                <span class="smc-chev">{ "\u{25be}" }</span>
            </button>
            <Show when=move || open.get() fallback=|| ()>
                <div
                    class="smc-backdrop"
                    on:click=move |e: MouseEvent| {
                        e.stop_propagation();
                        open.set(false);
                    }
                />
                <div
                    class="smc-panel sess-model-panel"
                    style=move || {
                        let (x, y) = pos.get();
                        format!("left:{x:.0}px; top:{y:.0}px;")
                    }
                    on:click=move |e: MouseEvent| e.stop_propagation()
                >
                    <div class="smc-sec">{ "model" }</div>
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
                                        class="smc-opt"
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
                                        <span class="smc-tick">{ "\u{2713}" }</span>
                                        { n.clone() }
                                    </button>
                                }
                                .into_any()
                            })
                            .collect();
                        opts
                    } }
                    <div class="smc-sec">{ "thinking effort" }</div>
                    <div class="smc-eff">
                        <button
                            class="smc-eff-pill"
                            class:on=move || current_effort.get().is_empty()
                            title="follow the model entry's own reasoning_effort"
                            on:click=move |_| {
                                let session = session_name.get_untracked();
                                spawn_local(async move {
                                    let _ = api::set_session_effort(&session, None).await;
                                    if let Ok(list) = api::load_sessions().await {
                                        state.sessions.set(list);
                                    }
                                });
                            }
                        >{ "inherit" }</button>
                        { move || {
                            let cur = current_effort.get();
                            levels
                                .get()
                                .into_iter()
                                .map(|v| {
                                    let on = v == cur;
                                    // `v` labels the pill, `val` rides into
                                    // the handler (one String each, like the
                                    // model rows above).
                                    let val = v.clone();
                                    let session = session_name.get_untracked();
                                    view! {
                                        <button
                                            class="smc-eff-pill"
                                            class:on=move || on
                                            on:click=move |_| {
                                                let session = session.clone();
                                                let val = val.clone();
                                                spawn_local(async move {
                                                    let _ = api::set_session_effort(
                                                        &session,
                                                        Some(val.as_str()),
                                                    )
                                                        .await;
                                                    if let Ok(list) = api::load_sessions().await {
                                                        state.sessions.set(list);
                                                    }
                                                });
                                            }
                                        >{ v.clone() }</button>
                                    }
                                })
                                .collect_view()
                        } }
                    </div>
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
    // v0.5.58: session-bound composer draft — stash the OUTGOING
    // session's unsent text (keyed by that session) before switching, so
    // it is restored when the user comes back and never carries into the
    // next session's input (where a send could land in the wrong session).
    if let Some(old_name) = state.active_session.get() {
        let text = state.draft.get();
        if !text.is_empty() {
            state.drafts.update(|m| {
                m.insert(old_name, text);
            });
        }
    }
    state.active_session.set(Some(name.to_string()));
    // v0.5.58: restore THIS session's own draft into the composer (empty
    // if it never had one), replacing whatever the previous session left
    // in the box. Persist the cross-session map so it survives a reload.
    state.draft.set(state.drafts.with(|m| m.get(name).cloned()).unwrap_or_default());
    crate::ui::persist_drafts(&state.drafts.get());
    // v0.5.55 P1: opening this session consumes its "finished, unviewed"
    // green lamp. Remember the view so the 10 s `loops` poll and the
    // connect-time `loops` frame do not re-seed the lamp for this session
    // (the server's marker deletion, fired below, covers reloads).
    state.loop_viewed.write().insert(name.to_string());
    state.events.set(Vec::new());
    // v0.5.52: window cleared — drop the derived signals too.
    state.clear_derived();
    state.view_round.set(None);
    state.ctx_used.set(0);
    state.rounds_ctxk.set(Vec::new());
    // v0.5.75: the meter panel is session-scoped — drop the previous
    // session's numbers AND its meta before the new fetch lands.
    state.ctx_cached.set(None);
    state.ctx_out.set(None);
    state.ctx_meta.set(None);
    state.ctx_panel_open.set(false);
    state.goal.set(None);
    // v0.5.57: drop the previous session's pinned context summary so the
    // card does not briefly show the OTHER session's compaction while the
    // new session's fetch is in flight.
    state.latest_compaction.set(None);
    // v0.5.38: drop the previous session's loop-cmd value so the chip
    // does not briefly show the OTHER session's command while the new
    // session's full-transcript fetch is in flight.
    state.loop_cmd.set(String::new());
    state.loop_cmd_sess.set(None);
    state.menu_session.set(None);
    state.clear_live();
    switch_panel_session(&state);

    let s2 = state;
    let name2 = name.to_string();
    spawn_local(async move {
        if let Ok(sessions) = api::load_sessions().await {
            s2.sync_session_bookkeeping(&sessions);
            s2.sessions.set(sessions);
        }
        ws::connect(&s2, &name2);
        // v0.5.55 P1: fire-and-forget — tell the server this session was
        // viewed so it deletes the `loop.last` marker and the green lamp
        // does not come back on the next poll or reload.
        api::mark_loop_viewed(&name2).await;
        s2.goal.set(api::load_goal(&name2).await);
        // v0.5.75: the meter's server-side numbers (system prompt size +
        // limits) ride the same session-switch fetch wave.
        s2.ctx_meta.set(api::load_context_meta(&name2).await);
        s2.loop_running.set(api::loop_running(&name2).await);
        // v0.5.57: pin the session's latest compaction summary (the current
        // context handoff) so it survives a page reload — it normally sits
        // deep in the event log, far outside the last-200 history window.
        s2.latest_compaction.set(api::load_latest_compaction(&name2).await);
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

/// M15: session switch — the right panel drops the session-scoped FILE
/// views but keeps every terminal tab: terminals are session-bound
/// (their ptys keep running in the server's session store), so a
/// switch must not `term_close` them or clear the xterm writer sinks.
/// The id counters stay alive too (a kept id must not be reused by
/// another session's new terminal), and the `term_state` lamps of the
/// kept terminals stay as they were — the reconnecting socket's
/// `term_status` frames resync them.
fn switch_panel_session(state: &AppState) {
    let active = state.rp_active.get();
    state.rp_tabs.update(|ts| {
        ts.retain(|t| t.kind != RpTabKind::File && t.kind != RpTabKind::Files);
    });
    state.rp_expanded.update(|s| s.clear());
    state.rp_children.set(std::collections::HashMap::new());
    state.rp_preview_map.set(std::collections::HashMap::new());
    state.rp_err_map.set(std::collections::HashMap::new());
    if !state.rp_tabs.get().iter().any(|t| t.id == active) {
        state.rp_active.set(state.rp_tabs.get().first().map(|t| t.id).unwrap_or(0));
    }
}

/// M15: session deletion — the session's terminals die with it (the
/// REST handler purges the server-side slots; here we drop the client
/// half: close any still-live pty, detach the writer sinks, remove the
/// tabs). File views of the deleted session (its File/Files tabs, when
/// it was the active one) are dropped with it.
fn teardown_panel_for_session(state: &AppState, deleted: &str) {
    for t in state
        .rp_tabs
        .get()
        .iter()
        .filter(|t| t.sess == deleted && t.kind == RpTabKind::Term)
    {
        ws::term_close(t.id);
        ws::clear_term_writer(t.id);
        state.term_state.update(|m| {
            m.remove(&t.id);
        });
    }
    state.rp_tabs.update(|ts| ts.retain(|t| t.sess != deleted));
    let active = state.rp_active.get();
    if !state.rp_tabs.get().iter().any(|t| t.id == active) {
        state.rp_active.set(state.rp_tabs.get().first().map(|t| t.id).unwrap_or(0));
    }
}

pub fn delete_session(state: AppState, name: &str) {
    let name_owned = name.to_string();
    let s2 = state;
    spawn_local(async move {
        match api::delete_session(&name_owned).await {
            Ok(()) => {
                // M15: a session's terminals are bound to it — deleting
                // the session kills its ptys (the REST handler already
                // purged the server slots; drop the client half here).
                teardown_panel_for_session(&s2, &name_owned);
                if s2.active_session.get().as_deref() == Some(name_owned.as_str()) {
                    s2.active_session.set(None);
                    s2.events.set(Vec::new());
                    s2.clear_derived();
                    s2.view_round.set(None);
                    s2.ctx_used.set(0);
                    s2.rounds_ctxk.set(Vec::new());
                    s2.ctx_cached.set(None);
                    s2.ctx_out.set(None);
                    s2.ctx_meta.set(None);
                    s2.ctx_panel_open.set(false);
                    s2.goal.set(None);
                    s2.clear_live();
                    // The panel's file views belonged to the now-gone
                    // session — drop them (terminal tabs of OTHER
                    // sessions survive, M15).
                    s2.rp_expanded.update(|s| s.clear());
                    s2.rp_children.set(std::collections::HashMap::new());
                    s2.rp_preview_map.set(std::collections::HashMap::new());
                    s2.rp_err_map.set(std::collections::HashMap::new());
                    ws::close_current();
                    s2.ws_status.set("disconnected".to_string());
                    // v0.5.58: the session is gone — drop its unsent draft and
                    // empty the box so it can't carry into the next selected
                    // session (or survive a reload).
                    s2.drafts.update(|m| {
                        m.remove(&name_owned);
                    });
                    s2.draft.set(String::new());
                    crate::ui::persist_drafts(&s2.drafts.get());
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

/// M7 (v0.5.56, v0.5.57): the project-group header line — the working
/// directory's label plus the card count. Shared by the sidebar's dispatch
/// view and the rewind History rail, which both group sessions by that path
/// (`model::dispatch_groups`).
///
/// The label is the path's basename (`dispatch_group_label`) or the user's
/// alias for that path (`group_label`), and **the basename itself carries the
/// full working path as its tooltip**: two projects can share a basename
/// (`…/rushi` and `…/rushi/rushi`), so hovering is the way to tell them apart.
///
/// Hovering the head reveals `✎` (`.dispatch-group-edit`), which swaps the
/// label for a compact input bound to `state.group_edit`. Enter/blur commits
/// via `set_group_label`, Escape cancels, an empty value clears the alias.
/// It is a **display-only** rename: the working path is never changed.
pub(crate) fn session_group_head(state: AppState, group_key: &str, count: usize) -> AnyView {
    // `StoredValue` (Copy): every closure below stays `Fn`, and the key — the
    // group's working path — is fixed for this head instance (the `For` key
    // includes it), so a plain copy is correct.
    let key = StoredValue::new(group_key.to_string());
    // The full working path. It is the tooltip and ONLY the tooltip: the
    // visible text is the label (basename or alias) and the path is never
    // rewritten.
    let title = key;
    let label = move || group_label(state, &key.get_value());
    // M7 renders a group's label uppercase; the user's own alias shows as
    // typed (`.custom`), which is what makes a rename feel like a rename.
    let label_cls = move || {
        let k = key.get_value();
        let custom = state
            .project_labels
            .get()
            .get(&k)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if custom {
            "dispatch-group-name custom"
        } else {
            "dispatch-group-name"
        }
    };
    let editing = move || state.group_edit.get().as_deref() == Some(key.get_value().as_str());
    let open_edit = move |e: MouseEvent| {
        e.stop_propagation();
        state.group_edit.set(Some(key.get_value()));
    };
    let commit = move |value: String| {
        set_group_label(state, &key.get_value(), &value);
        state.group_edit.set(None);
    };
    let cancel = move |_| state.group_edit.set(None);

    view! {
        <div class="dispatch-group-head">
            <Show
                when=editing
                fallback=move || {
                    view! {
                        <span class=label_cls title=title.get_value()>
                            { label }
                        </span>
                        <button
                            class="dispatch-group-edit"
                            title="rename this group's label (display only \u{2014} the working path is not changed)"
                            on:click=open_edit
                        >
                            { "\u{270E}" }
                        </button>
                    }
                }
            >
                <input
                    class="dispatch-group-input"
                    type="text"
                    placeholder=move || dispatch_group_label(&key.get_value())
                    prop:value=move || group_label(state, &key.get_value())
                    on:keydown=move |e: web_sys::KeyboardEvent| {
                        match e.key().as_str() {
                            "Enter" => {
                                e.prevent_default();
                                let v = e
                                    .target()
                                    .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                                    .map(|i| i.value())
                                    .unwrap_or_default();
                                commit(v);
                            }
                            "Escape" => {
                                e.prevent_default();
                                state.group_edit.set(None);
                            }
                            _ => {}
                        }
                    }
                    on:blur=move |e: web_sys::FocusEvent| {
                        let v = e
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                            .map(|i| i.value())
                            .unwrap_or_default();
                        commit(v);
                    }
                />
                <button class="dispatch-group-edit on" title="cancel" on:click=cancel>
                    { "\u{2715}" }
                </button>
            </Show>
            <span class="dispatch-group-count">{ count }</span>
        </div>
    }
    .into_any()
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
    let count = group_sessions.len();
    view! {
        <div class="dispatch-group">
            { session_group_head(state, &group_key, count) }
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
    session_card(state, s, false)
}

/// M7 (v0.5.56): the session card itself, shared by the sidebar's dispatch
/// view and the rewind plugin's History rail. It carries the session name,
/// the last-output time, the **per-session loop toggle** (`\u{25B6} start` /
/// `\u{25A0} stop`, over REST, for any session) and the `\u{2026}` menu
/// (rename / delete).
///
/// `stay` is the only behavioural difference: the dispatch view enters the
/// session and returns to the "split" layout, while the History rail keeps
/// the full-window view and lets the tree reload for the new session in
/// place (the C2 decision: switching sessions stays full-screen).
pub(crate) fn session_card(state: AppState, s: SessionInfo, stay: bool) -> AnyView {
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
                // M7: entering a session from the dispatch view returns to
                // the split layout; the History rail (stay = true) keeps the
                // full-window view — only the tree's session changes.
                select_session(state, &name_click);
                menu_session.set(None);
                if !stay && layout.get() == "full" {
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
    .into_any()
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
    // v0.5.58b (plan A): sending CONSUMES the session's stored draft. The
    // send handler already cleared the live box optimistically; this drops
    // the persisted copy so the just-sent text does not reappear in the
    // box when the user leaves and comes back (or after a reload). Note:
    // if the HTTP fallback below fails, the text is also gone from the
    // draft (consistent with the box already being cleared) — the user is
    // alerted "send failed".
    state.drafts.update(|m| {
        m.remove(&active);
    });
    crate::ui::persist_drafts(&state.drafts.get());
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
    // v0.5.52: keep the derived signals current for the optimistic card
    // (a user_message ⇒ the loop-cmd chip may change).
    state.note_event(&ev);
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
                <PluginArea state=state />
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

// ── sidebar plugin area (v0.5.53) ─────────────────────────────────
// v0.5.53: the sidebar's lower slot is a generic plugin area. A thin
// switch bar (‹ [plugin ▾] ›) sits above the active plugin's view; the
// shared #plugin-area chrome carries the height cap so one long prompt
// can never crush the session list. Plugins register in plugins.rs
// (compile-time registry + one match arm in plugin_view below).
#[component]
pub fn PluginArea(state: AppState) -> impl IntoView {
    let active = state.active_plugin;
    let menu_open = state.plugin_menu_open;
    let n_plugins = crate::plugins::count();

    view! {
        <div id="plugin-area">
            <div id="plugin-bar">
                <button
                    id="plugin-prev"
                    title="previous plugin"
                    disabled={n_plugins <= 1}
                    on:click=move |_| {
                        let cur = active.get();
                        after_dispatch(move || {
                            active.set(crate::plugins::prev_id(cur.as_str()).to_string());
                            menu_open.set(false);
                        });
                    }
                >
                    { "\u{2039}" }
                </button>
                <button
                    id="plugin-list"
                    title="switch plugin"
                    on:click=move |_| {
                        let open = !menu_open.get();
                        after_dispatch(move || menu_open.set(open));
                    }
                >
                    { move || crate::plugins::label(active.get().as_str()) }
                    <span class="plugin-caret">{ "\u{25BE}" }</span>
                </button>
                <button
                    id="plugin-next"
                    title="next plugin"
                    disabled={n_plugins <= 1}
                    on:click=move |_| {
                        let cur = active.get();
                        after_dispatch(move || {
                            active.set(crate::plugins::next_id(cur.as_str()).to_string());
                            menu_open.set(false);
                        });
                    }
                >
                    { "\u{203A}" }
                </button>
                <Show when=move || menu_open.get() fallback=|| ()>
                    <div
                        id="plugin-menu"
                        on:click=move |e: MouseEvent| { e.stop_propagation(); }
                    >
                        <For
                            each=move || crate::plugins::PLUGINS.to_vec()
                            key=|p: &PluginDef| p.id
                            children=move |p: PluginDef| {
                                let label = p.label;
                                view! {
                                    <button
                                        class=move || {
                                            if active.get() == p.id { "plugin-item on" } else { "plugin-item" }
                                        }
                                        on:click=move |_| {
                                            let id = p.id.to_string();
                                            after_dispatch(move || {
                                                active.set(id);
                                                menu_open.set(false);
                                            });
                                        }
                                    >
                                        { label }
                                    </button>
                                }
                            }
                        />
                    </div>
                </Show>
            </div>
            <div id="plugin-view">
                { move || plugin_view(active, state) }
            </div>
        </div>
    }
}

/// Render the active plugin's view into #plugin-view — one match arm per
/// registered plugin (registry: plugins.rs).
fn plugin_view(active: RwSignal<String>, state: AppState) -> AnyView {
    match active.get().as_str() {
        "goal" => goal_plugin_view(state).into_any(),
        "essence" => essence_plugin_view(state).into_any(),
        "rewind" => crate::rewind::rewind_plugin_view(state).into_any(),
        "time" => crate::time::time_plugin_view(state).into_any(),
        _ => view! { <div class="plugin-empty">{ "plugin not found" }</div> }.into_any(),
    }
}

/// The goal plugin — the first registered plugin. It renders the goal
/// prompt, status badge, meta line and the new/pause/resume/clear actions.
/// The height cap + switch bar live in the shared #plugin-area chrome, so
/// this view is content-only: no outer panel, no header (the bar's list
/// button shows the plugin name, in the former goal-header typography).
fn goal_plugin_view(state: AppState) -> impl IntoView {
    let goal = state.goal;

    view! {
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
    }
}

/// v0.5.54: the essence plugin — a read-only display of the active session's
/// session-essence entries (invariants + beliefs). Data comes from the
/// harness's `essence` store via GET /api/sessions/{id}/essence; refetched
/// on mount and whenever the active session changes. Content-only: no outer
/// panel / header — the shared #plugin-area chrome carries the cap + bar.
fn essence_plugin_view(state: AppState) -> impl IntoView {
    let entries = RwSignal::new(Option::<Vec<crate::model::EssenceEntry>>::None);
    let sess = state.active_session;

    Effect::new(move || {
        let s = match sess.get() {
            Some(s) => s.clone(),
            None => {
                entries.set(None);
                return;
            }
        };
        let ent = entries;
        spawn_local(async move {
            let v = api::load_essence(&s).await.unwrap_or_default();
            ent.set(Some(v));
        });
    });

    view! {
        <div id="essence-body">
            { move || match entries.get() {
                None => view! { <div class="essence-loading">{ "loading\u{2026}" }</div> }.into_any(),
                Some(es) if es.is_empty() => view! { <div class="essence-empty">{ "no essence entries yet" }</div> }.into_any(),
                Some(es) => {
                    let inv: Vec<crate::model::EssenceEntry> = es.iter().filter(|e| e.kind == "invariant").cloned().collect();
                    let bel: Vec<crate::model::EssenceEntry> = es.iter().filter(|e| e.kind == "belief").cloned().collect();
                    let inv_active = inv.iter().filter(|e| e.active).count();
                    let bel_active = bel.iter().filter(|e| e.active).count();
                    let inv_total = inv.len();
                    let bel_total = bel.len();
                    view! {
                        <div class="essence-groups">
                            <div class="essence-group">
                                <div class="essence-group-head">
                                    <span class="essence-group-label inv">{ "invariants" }</span>
                                    <span class="essence-count">{ format!("{inv_active}/{inv_total} active") }</span>
                                </div>
                                <For
                                    each=move || inv.clone()
                                    key=|e: &crate::model::EssenceEntry| e.id.clone()
                                    children=move |e: crate::model::EssenceEntry| essence_entry_view(e)
                                />
                            </div>
                            <div class="essence-group">
                                <div class="essence-group-head">
                                    <span class="essence-group-label bel">{ "beliefs" }</span>
                                    <span class="essence-count">{ format!("{bel_active}/{bel_total} active") }</span>
                                </div>
                                <For
                                    each=move || bel.clone()
                                    key=|e: &crate::model::EssenceEntry| e.id.clone()
                                    children=move |e: crate::model::EssenceEntry| essence_entry_view(e)
                                />
                            </div>
                        </div>
                    }.into_any()
                }
            } }
        </div>
    }
}

/// One essence entry: an id chip + an active/demoted state chip + the text.
fn essence_entry_view(e: crate::model::EssenceEntry) -> impl IntoView {
    let id = e.id.clone();
    let text = e.text.clone();
    let active = e.active;
    view! {
        <div class=move || if active { "essence-entry on" } else { "essence-entry" }>
            <div class="essence-entry-head">
                <span class="essence-id">{ id }</span>
                <span class=move || if active { "essence-state on" } else { "essence-state" }>{ move || if active { "active" } else { "demoted" } }</span>
            </div>
            <div class="essence-text">{ text }</div>
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

/// v0.5.75: the meter's budget. The kernel's `[limits].context_budget_tokens`
/// wins when the session's config was readable — that is the number the
/// loop actually compacts against. Otherwise v0.5.46 behaviour: the active
/// session's per-model entry, else the shared fallback.
pub(crate) fn ctx_budget(state: AppState) -> u64 {
    if let Some(b) = state.ctx_meta.get().and_then(|m| m.budget) {
        if b > 0 {
            return b;
        }
    }
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
}

/// Used / budget in percent, clamped to 100.
fn ctx_percent(state: AppState) -> f64 {
    let used = state.ctx_used.get() as f64;
    let budget = ctx_budget(state).max(1) as f64;
    (used / budget * 100.0).min(100.0)
}

/// dsh's compact count for the panel's own rows (`formatTokens` in
/// `dsh-client-ui-chat`): below 1K the number stays exact — `format_ctx`
/// would round a 283-token system prompt to "0K".
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        return if k >= 100.0 {
            format!("{}K", k.round() as u64)
        } else {
            format!("{}K", (k * 10.0).round() / 10.0)
        };
    }
    let m = n as f64 / 1_000_000.0;
    format!("{}M", (m * 10.0).round() / 10.0)
}

/// Thousands-separated exact count (`242432` → `242,432`) — the exact
/// figures dsh prints in its per-turn rows, beside the compact `~242K`.
fn fmt_exact(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// dsh's density heuristic (`dsh-token-meter`: `CHARS_PER_TOKEN = 4`,
/// system prompt `ceil(chars / 4) + 4`, per-block overhead). The messages
/// run denser here: these transcripts mix CJK, where 4 chars/token
/// under-counts, so 3.5 keeps the estimate honest.
const CHARS_PER_TOKEN: f64 = 3.5;
const SYSTEM_CHARS_PER_TOKEN: u64 = 4;

/// The prompt's composition as far as it can be known from the browser.
#[derive(Clone, Copy)]
struct Breakdown {
    system: u64,
    tools: u64,
    messages: u64,
}

/// Estimate the composition (v0.5.75). The event log carries no request
/// envelope — no system prompt, no tool schemas, no chat template — so
/// only the messages are countable. The provider-reported prompt size
/// (`ctx_used`) anchors the total; the system prompt is priced from the
/// server's character count; the residue becomes the third segment
/// ("tools & injects"), which is exactly where tool schemas and the
/// kernel's hook injections land. **The three always add up to `used`** —
/// the panel can never contradict the bar.
fn compute_breakdown(state: AppState) -> Breakdown {
    let used = state.ctx_used.get();
    // One pass over the loaded window. `chars_after_first` deliberately
    // starts at the window's first assistant message: its `input_tokens`
    // already prices everything that came before it (including the prompt
    // prefix and every round outside the window), so the prompt that
    // precedes it must NOT be counted again from text.
    let (chars_all, chars_after_first, first_in) = state.events.with(|evs| {
        let mut chars_all: u64 = 0;
        let mut chars_after: u64 = 0;
        let mut first_in: Option<u64> = None;
        let mut seen_first = false;
        for ev in evs.iter() {
            let t = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let mut n: u64 = 0;
            match t {
                "user_message" => {
                    if let Some(c) = ev.get("content").and_then(|v| v.as_str()) {
                        n = c.chars().count() as u64;
                    }
                }
                "assistant_message" => {
                    if let Some(c) = ev.get("content").and_then(|v| v.as_str()) {
                        n = c.chars().count() as u64;
                    }
                    if let Some(tcs) = ev.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            if let Some(args) = tc.get("arguments") {
                                n += args.to_string().chars().count() as u64;
                            }
                        }
                    }
                }
                "tool_result" => {
                    if let Some(c) = ev
                        .get("value")
                        .and_then(|v| v.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        n = c.chars().count() as u64;
                    }
                }
                _ => {}
            }
            chars_all += n;
            if seen_first {
                chars_after += n;
            }
            if t == "assistant_message" && first_in.is_none() {
                first_in = ev
                    .get("usage")
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|n| n.as_u64())
                    .filter(|n| *n > 0);
                seen_first = true;
            }
        }
        (chars_all, chars_after, first_in)
    });
    let est = |chars: u64| (chars as f64 / CHARS_PER_TOKEN).ceil() as u64;
    // Windowed history: anchor on the first assistant usage inside the
    // window instead of guessing from the round count (rounds vary far
    // too much in size). Everything the window is missing is already
    // inside that number.
    let mut messages = match (state.hist_has_more.get(), first_in) {
        (true, Some(first)) => first.saturating_add(est(chars_after_first)),
        _ => est(chars_all),
    };
    let mut system = state
        .ctx_meta
        .get()
        .and_then(|m| m.system_chars)
        .map(|c| c / SYSTEM_CHARS_PER_TOKEN + 4)
        .unwrap_or(0);
    if system + messages > used {
        // A very dense window can overshoot: scale the two estimates down
        // so the bar stays a decomposition of `used`, never a rival to it.
        let total = system + messages;
        if total == 0 {
            system = 0;
            messages = 0;
        } else {
            system = (system as u128 * used as u128 / total as u128) as u64;
            messages = used - system;
        }
    }
    let tools = used.saturating_sub(system + messages);
    Breakdown {
        system,
        tools,
        messages,
    }
}

/// The panel's width, dsh's rule: `min(264px, 100vw - 24px)`.
fn panel_width(vw: f64) -> f64 {
    264.0_f64.min((vw - 24.0).max(120.0))
}

/// Place the panel under the button's right edge (the meter sits at the
/// right end of the top band, so anchoring there keeps it attached) and
/// clamp into the viewport — dsh's `useAnchoredPosition` with
/// `side: "bottom"`, `gap: 8`, `margin: 12`. No flip: the top band always
/// has room below it.
fn anchor_panel(state: AppState) -> (f64, f64) {
    let Some(win) = web_sys::window() else {
        return (12.0, 12.0);
    };
    let vw = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1280.0);
    let vh = win
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(800.0);
    let Some(doc) = win.document() else {
        return (12.0, 12.0);
    };
    let Some(btn) = doc.get_element_by_id("ctx-k-btn") else {
        return (12.0, 12.0);
    };
    let rect = btn.get_bounding_client_rect();
    let width = panel_width(vw);
    let mut left = rect.right() - width;
    left = left.clamp(12.0, (vw - width - 12.0).max(12.0));
    let mut top = rect.bottom() + 8.0;
    if let Some(panel) = doc.get_element_by_id("ctx-panel") {
        let h = panel.get_bounding_client_rect().height();
        if h > 0.0 {
            top = top.clamp(12.0, (vh - h - 12.0).max(12.0));
        }
    }
    let _ = state;
    (left, top)
}

thread_local! {
    /// The meter panel's document listeners (outside `pointerdown`, Escape,
    /// `resize`). Held as `into_js_value` results, NOT as cloned
    /// `js_sys::Function`s: dropping a `Closure` invalidates the JS
    /// function it produced (the "closure invoked after being dropped"
    /// trap this codebase already documented), so the closure has to be
    /// leaked deliberately. A page runs one App, so a process-lifetime
    /// keep is safe.
    static CTX_PANEL_LISTENERS: std::cell::RefCell<Vec<wasm_bindgen::JsValue>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Install the panel's document listeners once. The outside-click test
/// spares the button itself: a `pointerdown` there is followed by the
/// button's own `click`, and closing on the former would fight the toggle.
fn ctx_panel_listeners(state: AppState) {
    if CTX_PANEL_LISTENERS.with(|c| !c.borrow().is_empty()) {
        return;
    }
    let Some(win) = web_sys::window() else { return };
    let Some(doc) = win.document() else { return };

    let hit = {
        let doc = doc.clone();
        move |id: &str, target: &web_sys::Node| -> bool {
            doc.get_element_by_id(id)
                .map(|el| el.unchecked_ref::<web_sys::Node>().contains(Some(target)))
                .unwrap_or(false)
        }
    };

    let s1 = state;
    let on_down = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        if !s1.ctx_panel_open.get_untracked() {
            return;
        }
        let Some(target) = e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()) else {
            return;
        };
        if !hit("ctx-panel", &target) && !hit("ctx-k-btn", &target) {
            s1.ctx_panel_open.set(false);
        }
    });
    let on_down_val = on_down.into_js_value();
    let _ = doc.add_event_listener_with_callback("pointerdown", on_down_val.unchecked_ref());

    let s2 = state;
    let on_key = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
        if e.key() == "Escape" && s2.ctx_panel_open.get_untracked() {
            s2.ctx_panel_open.set(false);
        }
    });
    let on_key_val = on_key.into_js_value();
    let _ = doc.add_event_listener_with_callback("keydown", on_key_val.unchecked_ref());

    let s3 = state;
    let on_resize = Closure::<dyn FnMut()>::new(move || {
        if s3.ctx_panel_open.get_untracked() {
            s3.ctx_panel_pos.set(anchor_panel(s3));
        }
    });
    let on_resize_val = on_resize.into_js_value();
    let _ = win.add_event_listener_with_callback("resize", on_resize_val.unchecked_ref());

    CTX_PANEL_LISTENERS.with(|c| {
        *c.borrow_mut() = vec![on_down_val, on_key_val, on_resize_val];
    });
}

/// One row of the panel's legend / last-turn list.
fn ctx_row(key: &'static str, hint: &'static str, value: String) -> AnyView {
    view! {
        <div class="ctx-prow" title=hint>
            <span class="ctx-pkey">{ key }</span>
            <span class="ctx-pval">{ value }</span>
        </div>
    }
    .into_any()
}

/// The panel's body. Built inside the `Show`, so every closure here runs
/// only while the panel is open — the closed meter costs nothing per
/// frame (the frame path stays read-only).
fn ctx_panel_body(state: AppState) -> AnyView {
    let used = move || state.ctx_used.get();
    let figures = move || format!("~{} / {}", format_ctx(used()), format_ctx(ctx_budget(state)));

    let segments = move || {
        let b = compute_breakdown(state);
        let total = (b.system + b.tools + b.messages).max(1);
        let pct = ctx_percent(state);
        let mk = |n: u64, cls: &'static str| -> Option<AnyView> {
            let w = pct * (n as f64) / (total as f64);
            if w <= 0.05 {
                return None;
            }
            Some(
                view! { <div class=format!("ctx-seg {cls}") style=format!("width:{w:.2}%") /> }
                    .into_any(),
            )
        };
        [
            mk(b.system, "ctx-tint-system"),
            mk(b.tools, "ctx-tint-tools"),
            mk(b.messages, "ctx-tint-messages"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<AnyView>>()
    };

    let mark = move || match state.ctx_meta.get() {
        Some(m) => match (m.budget, m.compact_reserve) {
            (Some(b), Some(r)) if b > 0 => {
                let p = ((b.saturating_sub(r)) as f64 / b as f64 * 100.0).clamp(0.0, 100.0);
                format!("left:{p:.2}%;")
            }
            _ => "display:none;".to_string(),
        },
        None => "display:none;".to_string(),
    };

    let zero = move || used() == 0;

    view! {
        <div class="ctx-phead">
            <span class="ctx-plit">{ move || format!("{:.1}%", ctx_percent(state)) }</span>
            <span class="ctx-psent">{ " of context used" }</span>
            <span class="ctx-pfig">{ figures }</span>
        </div>
        <div class="ctx-pbar">
            <div class="ctx-pmark" style=mark />
            { segments }
        </div>
        <div class="ctx-prows">
            { move || {
                let b = compute_breakdown(state);
                vec![
                    ctx_row("System prompt", "priced from the kernel config (chars / 4 + 4)", format!("~{}", fmt_tokens(b.system))),
                    ctx_row(
                        if state.hist_has_more.get() { "Earlier rounds + tools" } else { "Tools & injects" },
                        "residual: tool schemas, hook injections, chat template — plus any rounds outside the loaded window",
                        format!("~{}", fmt_tokens(b.tools)),
                    ),
                    ctx_row("Messages", "estimated at ~3.5 chars/token over the loaded window", format!("~{}", fmt_tokens(b.messages))),
                ]
            } }
        </div>
        <Show when=move || state.hist_has_more.get() fallback=|| ()>
            <div class="ctx-pnote">
                { "only the newest rounds are loaded — Messages is anchored to the first assistant usage in the window, which already prices everything older" }
            </div>
        </Show>
        <div class="ctx-prule" />
        <div class="ctx-pcap">{ "last turn" }</div>
        <div class="ctx-prows">
            <Show when=move || !zero() fallback=|| ()>
                <Show
                    when=move || { state.ctx_cached.get().is_some() && used() > 0 }
                    fallback=|| ()
                >
                    { move || {
                        let c = state.ctx_cached.get().unwrap_or(0);
                        let pct = c as f64 / used().max(1) as f64 * 100.0;
                        ctx_row("Cache hit", "share of the prompt served from the prompt cache", format!("{pct:.1}%"))
                    } }
                </Show>
                <Show when=move || state.ctx_cached.get().is_some() fallback=|| ()>
                    { move || {
                        let c = state.ctx_cached.get().unwrap_or(0);
                        ctx_row("Uncached input", "prompt tokens the provider billed as new", fmt_exact(used().saturating_sub(c)))
                    } }
                </Show>
                <Show when=move || state.ctx_cached.get().is_some() fallback=|| ()>
                    { move || ctx_row("Cache read", "prompt tokens served from the prompt cache", fmt_exact(state.ctx_cached.get().unwrap_or(0))) }
                </Show>
                <Show when=move || state.ctx_out.get().is_some() fallback=|| ()>
                    { move || ctx_row("Output", "tokens generated in the last turn", fmt_exact(state.ctx_out.get().unwrap_or(0))) }
                </Show>
            </Show>
            <Show when=zero fallback=|| ()>
                <div class="ctx-pempty">{ "no usage recorded yet" }</div>
            </Show>
        </div>
    }
    .into_any()
}

#[component]
pub fn ContextBar(state: AppState) -> impl IntoView {
    let ctx_used = state.ctx_used;
    let view_round = state.view_round;
    let events = state.events;
    let layout = state.layout_mode;
    let panel_open = state.ctx_panel_open;
    let panel_pos = state.ctx_panel_pos;

    // v0.5.75: the meter panel's document listeners (outside click /
    // Escape / resize). Installed once, for the app's lifetime.
    ctx_panel_listeners(state);

    // Re-anchor after the content changes: the panel's height is only
    // known once it is in the DOM, and the clamp depends on it. Runs only
    // while open (the early return), so the closed meter is free.
    Effect::new(move |_| {
        if !panel_open.get() {
            return;
        }
        let _ = state.ctx_meta.get();
        let _ = ctx_used.get();
        let _ = state.ctx_cached.get();
        let _ = state.ctx_out.get();
        state.events.with(|_| ());
        panel_pos.set(anchor_panel(state));
    });

    let budget = move || ctx_budget(state);

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

    let ctx_pct = move || format!("{:.1}%", ctx_percent(state));

    let ctx_k = move || format!("~{} / {}", format_ctx(ctx_used.get()), format_ctx(budget()));

    // The compaction line: the kernel compacts once the prompt reaches
    // `budget - reserve`. Hidden when the server gave us no `[limits]`;
    // it turns --warn the moment the fill passes it.
    let mark_style = move || match state.ctx_meta.get() {
        Some(m) => match (m.budget, m.compact_reserve) {
            (Some(b), Some(r)) if b > 0 => {
                let p = ((b.saturating_sub(r)) as f64 / b as f64 * 100.0).clamp(0.0, 100.0);
                let hot = (ctx_used.get() as f64 / b as f64 * 100.0) >= p;
                if hot {
                    format!("left:{p:.2}%; background:var(--warn);")
                } else {
                    format!("left:{p:.2}%;")
                }
            }
            _ => "display:none;".to_string(),
        },
        None => "display:none;".to_string(),
    };
    let mark_title = move || match state.ctx_meta.get() {
        Some(m) => match (m.budget, m.compact_reserve) {
            (Some(b), Some(r)) if b > 0 => format!(
                "auto-compaction starts at {} (budget {} - reserve {})",
                format_ctx(b.saturating_sub(r)),
                format_ctx(b),
                format_ctx(r)
            ),
            _ => String::new(),
        },
        None => String::new(),
    };

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
                <div id="ctx-track" title=mark_title>
                    <div id="ctx-fill" style=fill_style />
                    <div id="ctx-mark" style=mark_style />
                </div>
                <span id="ctx-pct">{ ctx_pct }</span>
                // v0.5.75: the figures ARE the meter's button now. Click
                // opens the composition panel (a port of dsh's
                // `ContextMeter`), outside click / Escape close it.
                <button
                    id="ctx-k-btn"
                    class=move || if panel_open.get() { "open" } else { "" }
                    title="context usage"
                    aria-haspopup="dialog"
                    aria-expanded=move || if panel_open.get() { "true" } else { "false" }
                    on:click=move |_| {
                        if panel_open.get() {
                            panel_open.set(false);
                        } else {
                            panel_open.set(true);
                            panel_pos.set(anchor_panel(state));
                        }
                    }
                >
                    { ctx_k }
                </button>
                // M8/M13: right tool panel toggle. Right edge of the
                // context bar, symmetric to the sidebar's own edge
                // buttons. M13: redesigned as a stateful "panel" icon
                // (the right column fills with the accent when open) on
                // a neomorphic 26×26 pill, replacing the old ▢/▣ glyph.
                <button
                    id="rp-toggle"
                    class=move || if state.rp_open.get() { "rp-toggle active" } else { "rp-toggle" }
                    title=move || {
                        if state.rp_open.get() {
                            "close the right panel".to_string()
                        } else {
                            "open the right panel".to_string()
                        }
                    }
                    on:click=move |_| toggle_rp(state)
                >
                    { move || rp_toggle_icon(state.rp_open.get()) }
                </button>
            </div>
            <div id="ctx-rounds">
                { move || rounds_view(events, view_round, state) }
            </div>
            <Show when=move || panel_open.get() fallback=|| ()>
                <div
                    id="ctx-panel"
                    role="dialog"
                    aria-label="context usage"
                    style=move || {
                        let (l, t) = panel_pos.get();
                        format!("left:{l:.0}px; top:{t:.0}px;")
                    }
                >
                    { ctx_panel_body(state) }
                </div>
            </Show>
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
    // v0.5.52: ONE borrow for everything this view needs from `events`.
    // `events.get()` deep-clones the whole event Vec and this view
    // re-runs on every appended event (O(N) per event — §9.13), so we
    // take the borrow and lift out only what is needed (the round
    // ranges + each round's user text).
    let (rounds, user_texts) = events.with(|evs| {
        let rounds = compute_rounds(evs);
        let texts: Vec<String> = rounds
            .iter()
            .map(|r| {
                evs.get(r.start)
                    .and_then(|e| e.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        (rounds, texts)
    });
    let n = rounds.len();
    let slots = 6.max(n);
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
            // v0.5.52: `rounds[i]` is no longer read here — the round's
            // user text is lifted out of the single borrow above.
            let user_text = user_texts.get(i).cloned().unwrap_or_default();
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
                            // v0.5.52: borrow instead of cloning the whole
                            // event Vec on every append (§9.13).
                            let m = state.hist_total_rounds.get().saturating_sub(
                                events.with(|v| compute_rounds(v).len()) as u64,
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
    view! {
        <div id="status-strip">
            // v0.5.52: the strip reads only the incrementally maintained
            // `last_ext` map (O(1) per appended event; no whole-window
            // scan and no view rebuild unless an id's value changed).
            { move || state.last_ext.with(|m| status_strip_chips(m)) }
        </div>
    }
}

/// v0.5.52: the PRE-P2 shape, kept verbatim for the `?test=freeze
/// mode=events flood=raw` NEGATIVE CONTROL — it walks the whole event
/// window and rebuilds every chip, which is what the app did on each
/// appended event before the incremental `last_ext` map. The control
/// only discriminates while this stays faithful, so do not "optimise"
/// it.
#[allow(dead_code)]
pub(crate) fn status_strip_chips_full(events: &[serde_json::Value]) -> AnyView {
    use std::collections::HashMap;
    let mut latest: HashMap<String, String> = HashMap::new();
    for ev in events {
        if ev.get("type").and_then(|v| v.as_str()) != Some("ext_status") {
            continue;
        }
        let Some(id) = ev.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let s = crate::model::ext_status_value_str(ev.get("value"));
        latest.insert(id.to_string(), s);
    }
    status_strip_chips(&latest)
}

/// v0.5.52: chips are built from the incrementally maintained
/// `AppState::last_ext` map (plan §9.13/§9.14 P2) — this used to scan
/// the whole event window and rebuild every chip on each appended event.
fn status_strip_chips(latest: &std::collections::HashMap<String, String>) -> AnyView {
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
    // v0.5.58: the composer text is session-bound — it lives in
    // `state.draft` (restored/stashed by `select_session` per session,
    // keyed by `state.drafts`), not a component-local signal, so it no
    // longer carries one session's words into another on a switch.
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
                    bind:value=state.draft
                    on:input=move |_| { size_msg_input(); }
                    on:keydown=move |e: KeyboardEvent| {
                        if e.key() == "Enter" && (e.ctrl_key() || e.meta_key()) {
                            e.prevent_default();
                            let q = qsel_value.get().clone();
                            let text = state.draft.get();
                            state.draft.set(String::new());
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
                        let text = state.draft.get();
                        state.draft.set(String::new());
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

/// M13: 16×16 "panel on the right" icon for the #rp-toggle button.
/// When the panel is open the right column is filled (accent), when it
/// is closed only the outline shows — so the control reads as a state
/// indicator, not just a static glyph.
fn rp_toggle_icon(open: bool) -> impl IntoView {
    view! {
        <svg class="rp-toggle-ic" viewBox="0 0 16 16" aria_hidden="true">
            <rect class="ln" x="1.5" y="2.5" width="13" height="11" rx="2.5" />
            <path class="ln" d="M10.5 2.5v11" />
            <rect
                class=move || {
                    if open { "rp-toggle-fill" } else { "rp-toggle-fill off" }
                }
                x="10.5"
                y="2.5"
                width="4"
                height="11"
            />
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

/// M13: open (or refocus) the Files tree tab. The Files tab has the
/// stable id 0; if it is already open we just focus it, otherwise we
/// (re)create it and focus it. Used by the panel start view's Files
/// launcher and the "+" menu.
fn open_files_tab(state: AppState) {
    if let Some(id) = state
        .rp_tabs
        .get()
        .iter()
        .find(|t| t.kind == RpTabKind::Files)
        .map(|t| t.id)
    {
        state.rp_active.set(id);
        return;
    }
    state.rp_tabs.update(|tabs| {
        tabs.push(RpTab {
            id: 0,
            kind: RpTabKind::Files,
            path: String::new(),
            label: "Files".to_string(),
            sess: String::new(),
        });
    });
    state.rp_active.set(0);
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
            sess: sess.clone(),
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
    // M15: the terminal is bound to the session it was opened in —
    // the label shows the binding, and the tab (with its server pty)
    // survives session switches until explicitly closed or the
    // session is deleted.
    let sess = match state.active_session.get() {
        Some(s) => s,
        None => return,
    };
    let label = format!("Term {seq} · {sess}");
    state.rp_tabs.update(|tabs| {
        tabs.push(RpTab {
            id,
            kind: RpTabKind::Term,
            path: String::new(),
            label,
            sess,
        });
    });
    state
        .term_state
        .update(|m| {
            m.entry(id).or_default();
        });
    state.rp_active.set(id);
}

/// M11/M13: close one tab. Terminal tabs close their pty on the server
/// (`term_close{id}` — the connection stays open, the other tabs'
/// shells keep running). File/Files tabs just drop their cached
/// preview/error. M13: the Files tab (id 0) is now closable too —
/// closing the last tab leaves the panel on its empty start view.
/// After closing the active tab, focus moves to the previous tab
/// (or the start view when none remain).
fn close_tab(state: AppState, id: u32) {
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

/// M8/M11/M13: the right tool panel — a fixed 420 px column in #app
/// (sibling of #main, so #main shrinks to make room). Browser-style
/// multi-tab: M13 starts on an EMPTY "start" view (no tabs) with two
/// launcher buttons (Files / Terminal); opening one creates that tab.
/// The "+" button only appears once a tab exists, and opens a small
/// dropdown (Files / Terminal) rather than spawning a terminal directly.
/// Every tab — including Files — is closable; closing the last one
/// returns the panel to the start view. Hidden in the "full" dispatch
/// layout and when the context-bar toggle is off.
#[component]
pub fn RightPanel(state: AppState) -> impl IntoView {
    let tabs = state.rp_tabs;
    let active_id = state.rp_active;
    let active_session = state.active_session;
    // M15: user-adjustable panel width (the `.rp-resizer` strip in
    // lib.rs writes `rp_width` on drag; the panel reads it through the
    // `--rp-w` custom property, default 420px).
    let rp_width = state.rp_width;
    // M13: the "+" dropdown's open state (local to this panel).
    let plus_menu = create_rw_signal(false);

    view! {
        <aside
            id="right-panel"
            style=move || format!("--rp-w: {}px", rp_width.get())
        >
            <div class="rp-tabbar">
                <div class="rp-tabs">
                    <For
                        each=move || tabs.get()
                        key=|t: &RpTab| t.id.to_string()
                        children=move |tab| {
                            let t_id = tab.id;
                            let t_kind = tab.kind;
                            let t_label = tab.label.clone();
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
                                    <button
                                        class="rp-tab-x"
                                        title="close tab"
                                        on:click=move |_| close_tab(state, t_id)
                                    >
                                        { "\u{00d7}" }
                                    </button>
                                </div>
                            }
                        }
                    />
                </div>
                // M13: the "+" button (dropdown) is only present once a tab
                // exists — the empty start view carries its own launchers.
                <Show
                    when=move || !tabs.get().is_empty()
                    fallback=|| ()
                >
                    <div class="rp-tabnew-wrap">
                        <button
                            class="rp-tab-new"
                            id="rp-tab-new"
                            title="new tab"
                            disabled=move || active_session.get().is_none()
                            on:click=move |_| plus_menu.set(!plus_menu.get())
                        >
                            { "+" }
                        </button>
                        <Show when=move || plus_menu.get() fallback=|| ()>
                            {
                                move || {
                                    if plus_menu.get() {
                                        Some(view! {
                                            <div class="rp-menu-backdrop"
                                                on:click=move |_| plus_menu.set(false) />
                                            <div class="rp-tabnew-menu" id="rp-tabnew-menu">
                                                <button
                                                    class="rp-menu-item"
                                                    on:click=move |_| {
                                                        plus_menu.set(false);
                                                        open_files_tab(state);
                                                    }
                                                >
                                                    { tab_icon_files() }
                                                    <span>Files</span>
                                                </button>
                                                <button
                                                    class="rp-menu-item"
                                                    on:click=move |_| {
                                                        plus_menu.set(false);
                                                        new_terminal(state);
                                                    }
                                                >
                                                    { tab_icon_term() }
                                                    <span>Terminal</span>
                                                </button>
                                            </div>
                                        })
                                    } else {
                                        None
                                    }
                                }
                            }
                        </Show>
                    </div>
                </Show>
            </div>
            <div class="rp-body">
                <For
                    each=move || tabs.get()
                    key=|t: &RpTab| t.id.to_string()
                    children=move |tab| tab_pane(state, tab)
                />
                // M13: the empty "start" view — two launcher buttons that
                // create the Files / Terminal tab the user picks.
                <Show when=move || tabs.get().is_empty() fallback=|| ()>
                    <div class="rp-start">
                        <div class="rp-start-hint">open a tool</div>
                        <button
                            class="rp-start-btn"
                            id="rp-start-files"
                            on:click=move |_| open_files_tab(state)
                        >
                            { tab_icon_files() }
                            <div class="rp-start-txt">
                                <div class="rp-start-name">Files</div>
                                <div class="rp-start-sub">directory tree &amp; previews</div>
                            </div>
                        </button>
                        <button
                            class="rp-start-btn"
                            id="rp-start-term"
                            disabled=move || active_session.get().is_none()
                            on:click=move |_| new_terminal(state)
                        >
                            { tab_icon_term() }
                            <div class="rp-start-txt">
                                <div class="rp-start-name">Terminal</div>
                                <div class="rp-start-sub">a live shell</div>
                            </div>
                        </button>
                    </div>
                </Show>
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
