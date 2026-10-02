//! The `time-inject` plugin's client side — the sidebar `time` panel (the
//! `rushi-time-inject` package).
//!
//! One control: a per-session on/off switch for the harness's
//! `time-inject` hook, which upserts the last-user-message timestamp and
//! the idle gap into every model call. The switch writes the session's
//! `.time_inject` marker through the plugin's own API
//! (`GET`/`POST /api/sessions/{id}/time-inject`), which the server
//! projects back ([`crate::model::TimeInjectView`]); the hook re-reads the
//! marker on its next model call, so a flip needs no loop restart.
//!
//! Registered in `plugins.rs` (`PLUGINS`) and dispatched from
//! `ui.rs::plugin_view` — the plugin's only two WebUI touch points. The
//! panel is content-only: the shared `#plugin-area` chrome provides the
//! height cap and the plugin bar.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::model::AppState;

/// v0.5.56: the time plugin — a per-session on/off toggle for the
/// harness's `time-inject` hook (the `rushi-time-inject` plugin). When
/// ON, the hook injects the last-user-message timestamp + idle gap into
/// every model call so the agent knows *when* a message arrived; when
/// OFF it stays silent. The toggle writes the session's `.time_inject`
/// marker, which the hook reads live on the next model call — no loop
/// restart.
///
/// The view is content-only (the shared #plugin-area chrome provides the
/// cap + switch bar): a single switch row and a status line describing
/// the effective state.
pub fn time_plugin_view(state: AppState) -> impl IntoView {
    let vi = state.time_inject;
    let sess = state.active_session;
    // Set when the marker fetch fails (e.g. the page outlived a server
    // restart and the running binary lacks the endpoint): the view then
    // shows a click-to-retry hint instead of an endless "loading…".
    let failed = RwSignal::new(false);

    // Fetch the current marker state when the active session changes.
    Effect::new(move || {
        let s = match sess.get() {
            Some(s) => s.clone(),
            None => {
                vi.set(None);
                failed.set(false);
                return;
            }
        };
        let t = vi;
        let f = failed;
        f.set(false);
        spawn_local(async move {
            match api::load_time_inject(&s).await {
                Ok(v) => t.set(Some(v)),
                Err(_) => f.set(true),
            }
        });
    });

    view! {
        <div id="time-body">
            { move || match (sess.get(), vi.get()) {
                (None, _) => view! { <div class="plugin-empty">{ "no session selected" }</div> }.into_any(),
                (_, None) => {
                    let f = failed;
                    view! {
                        <div
                            class=move || if f.get() { "time-error" } else { "time-loading" }
                            on:click=move |_| {
                                let Some(id) = sess.get() else {
                                    return;
                                };
                                let t = vi;
                                let ff = f;
                                ff.set(false);
                                spawn_local(async move {
                                    match api::load_time_inject(&id).await {
                                        Ok(v) => t.set(Some(v)),
                                        Err(_) => ff.set(true),
                                    }
                                });
                            }
                        >{ move || if f.get() { "load failed — click to retry" } else { "loading\u{2026}" } }</div>
                    }.into_any()
                }
                (_, Some(v)) => {
                    let on = v.enabled;
                    let t = vi;
                    let s = sess;
                    view! {
                        <div class="time-row">
                            <span class="time-label">time inject</span>
                            <button
                                class=move || if on { "time-switch on" } else { "time-switch" }
                                on:click=move |_| {
                                    let next = !on;
                                    let Some(id) = s.get() else {
                                        return;
                                    };
                                    let tt = t;
                                    spawn_local(async move {
                                        if let Ok(nv) = api::set_time_inject(&id, next).await {
                                            tt.set(Some(nv));
                                        }
                                    });
                                }
                            >
                                <span class="time-switch-knob"/>
                            </button>
                        </div>
                        <div class="time-status">
                            { move || if on {
                                "on — the agent is told when your last message was sent and how long the session sat idle; time-sensitive state is re-verified on return."
                            } else {
                                "off — no time info is injected; the agent has no sense of when your messages arrive."
                            } }
                        </div>
                    }.into_any()
                }
            } }
        </div>
    }
}
