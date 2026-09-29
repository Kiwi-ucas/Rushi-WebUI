//! Rushi Web UI — Leptos WASM SPA (cdylib).

mod api;
mod markdown;
mod model;
mod pile;
mod timeutil;
mod transcript;
mod ui;
mod ws;

/// v0.5.40: build-time webui version, injected by `build.rs`
/// (env override → latest `vX.Y.Z` commit subject → `dev-<sha>` →
/// "dev"). Shown in the sidebar header and the boot-time console
/// marker — one source of truth, so the displayed version can no
/// longer drift behind the tree. `env!` is a compile-time literal;
/// `build.rs` guarantees the variable is always set, so this cannot
/// panic.
pub const WEBUI_VERSION: &str = env!("RUSHI_WEBUI_VERSION");

use leptos::prelude::*;
use leptos::task::spawn_local;

/// Root component: builds app layout + wires up background polling.
#[component]
fn App() -> impl IntoView {
    let state = model::AppState::new();

    // v0.5.23: publish the live state so out-of-tree code (the
    // ?test=freeze regression test, console diagnostics) can reach
    // the app's signals.
    model::AppState::set_app_state(state);

    // Restore persisted sidebar-collapse state
    {
        let collapsed = ui::read_collapsed();
        state.sidebar_collapsed.set(collapsed);
    }

    // v0.5.22: theme (auto/light/dark). The pre-paint inline script in
    // index.html already set <html data-theme> for the first paint;
    // this restores the persisted mode into the signal, re-applies the
    // attribute, and follows OS scheme changes in auto mode.
    ui::theme_init(state);

    // v0.5.30: restore the persisted sidebar ordering (mode + the
    // user's own drag order). The "output" rank map seeds itself
    // from the first session load below (last_modified baseline).
    {
        let (mode, order) = ui::read_persisted_sort();
        state.sort_mode.set(mode);
        state.custom_order.set(order);
    }

    // v0.5.38: keep the loop-cmd chip's `loop_cmd` signal fresh from
    // the loaded event window. The WS history frame only carries the
    // last HIST_PAGE events, but a freshly sent command is always the
    // most recent user_message, so mirroring it whenever the window has
    // one keeps the chip correct without any fetch. A session whose
    // command is BURIED under >HIST_PAGE events is seeded separately by
    // a full-transcript fetch (ui.rs select_session + the s3 poll below).
    {
        let st = state;
        Effect::new(move || {
            let s = model::last_user_command_slice(&st.events.get());
            if !s.is_empty() {
                st.loop_cmd.set(s);
            }
        });
    }

    // Background: initial session load + periodic polling (port of JS setInterval loops)
    let s1 = state;
    spawn_local(async move {
        if let Ok(sessions) = api::load_sessions().await {
            // v0.5.30: reconcile the sidebar-ordering bookkeeping
            // (output-rank seeding + custom-order membership) BEFORE
            // the list is rendered.
            s1.sync_session_bookkeeping(&sessions);
            s1.sessions.set(sessions);
        }
        loop {
            gloo_timers::future::TimeoutFuture::new(10_000).await;
            if let Ok(sessions) = api::load_sessions().await {
                s1.sync_session_bookkeeping(&sessions);
                s1.sessions.set(sessions);
            }
        }
    });

    let s2 = state;
    spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(10_000).await;
            if let Some(id) = s2.active_session.get() {
                s2.goal.set(api::load_goal(&id).await);
            }
        }
    });

    let s3 = state;
    spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(4_000).await;
            if let Some(id) = s3.active_session.get() {
                s3.loop_running.set(api::loop_running(&id).await);
                // v0.5.38: reseed the loop-cmd chip when this poll is the
                // first for the active session (select_session's own fetch
                // may have been skipped — e.g. the loop was not running at
                // switch time — or may have failed). Only while a loop
                // runs, and only when `loop_cmd_sess` has not yet been
                // claimed by this session. A freshly sent command is
                // always in the loaded window and is mirrored into
                // `loop_cmd` by the Effect in App(), so this full fetch
                // only matters for a command BURIED under >HIST_PAGE
                // model/tool events.
                if s3.loop_running.get()
                    && s3.loop_cmd_sess.get().as_deref() != Some(id.as_str())
                {
                    if let Ok(evs) = api::load_events(&id).await {
                        s3.loop_cmd
                            .set(model::last_user_command_slice(&evs));
                    }
                    s3.loop_cmd_sess.set(Some(id.clone()));
                }
            }
        }
    });

    // v0.5.23: freeze regression test (?test=freeze): ~1.5s after
    // mount, drive a synthetic model-stream flood through the real
    // coalescing path and report a PASS/FAIL verdict (console +
    // #rushi-freeze-badge + window.__rushiFreezeResult). Add
    // &flood=raw to emulate the PRE-fix per-delta behaviour instead
    // (the negative control — expected to FAIL).
    {
        let search = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default();
        if search.contains("test=freeze") {
            spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(1_500).await;
                crate::ws::run_freeze_test();
            });
        }
    }

    let app_class = move || {
        if state.sidebar_collapsed.get() { "sidebar-collapsed".to_string() } else { String::new() }
    };

    view! {
        <div id="app" class=app_class>
            <ui::Sidebar state=state />
            <main id="main">
                <ui::ContextBar state=state />
                <transcript::Transcript state=state />
                <Show
                    when=move || state.active_session.get().is_none()
                    fallback=|| ()
                >
                    <ui::Welcome state=state />
                </Show>
                // v0.5.15: loop-in-progress marker in the gap between
                // the last card and the input box. Three-ring "breathing"
                // (ported from the castepsui homepage logo, re-styled
                // into this theme's relief language): three concentric
                // rings; ONE raised bulge (the theme's --shadow) travels
                // outer -> mid -> inner on staggered delays, reading as
                // swell/shrink. Colors, inner -> outer: deep green /
                // deep beige / salmon orange.
                <Show when=move || state.loop_running.get() fallback=|| ()>
                    <div id="loop-indicator">
                        <div class="loop-rings">
                            <div class="loop-ring lg-out" />
                            <div class="loop-ring lg-mid" />
                            <div class="loop-ring lg-in" />
                        </div>
                        // v0.5.38: recessed (sunken relief) chip right of the
                        // rings: the instruction the user most recently sent,
                        // held in `loop_cmd` (mirrored from the loaded event
                        // window, seeded by a full fetch when buried). A
                        // reminder for when the user forgets what they asked
                        // while waiting on a long loop. Inside the same
                        // <Show>, so it appears and disappears WITH the rings.
                        <Show
                            when=move || !state.loop_cmd.get().is_empty()
                            fallback=|| ()
                        >
                            <div
                                class="loop-cmd"
                                title={move || state.loop_cmd.get()}
                            >
                                {move || state.loop_cmd.get()}
                            </div>
                        </Show>
                    </div>
                </Show>
                <ui::InputModule state=state />
            </main>
            <ui::NewSessionDialog state=state />
            <ui::DeleteConfirmDialog state=state />
        </div>
    }
}

/// WASM entry point: mount the app into `<div id="mount-root">`.
#[cfg(target_arch = "wasm32")]
mod entry {
    use crate::App;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    #[wasm_bindgen(start)]
    pub fn main() {
        // Panic diagnostics: a wasm panic inside an rAF/event callback
        // would otherwise silently kill the pile engine's loop.
        // Capture the last one for __rushiPile() and mirror to console.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload: &str = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("unknown panic");
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_default();
            let msg = format!("wasm panic: {payload} at {loc}");
            crate::pile::record_panic(&msg);
            let _ = js_sys::eval(&format!("console.error({:?})", msg));
            default_hook(info);
        }));
        let document = web_sys::window().unwrap().document().unwrap();
        let el: web_sys::HtmlElement = document
            .get_element_by_id("mount-root")
            .expect("#mount-root")
            .dyn_into()
            .expect("mount-root is an HtmlElement");
        leptos::mount::mount_to(el, || App()).forget();
    }
}
