//! Rushi Web UI — Leptos WASM SPA (cdylib).

mod api;
mod markdown;
mod model;
mod pile;
mod timeutil;
mod transcript;
mod ui;
mod ws;

use leptos::prelude::*;
use leptos::task::spawn_local;

/// Root component: builds app layout + wires up background polling.
#[component]
fn App() -> impl IntoView {
    let state = model::AppState::new();

    // Restore persisted sidebar-collapse state
    {
        let collapsed = ui::read_collapsed();
        state.sidebar_collapsed.set(collapsed);
    }

    // Background: initial session load + periodic polling (port of JS setInterval loops)
    let s1 = state;
    spawn_local(async move {
        if let Ok(sessions) = api::load_sessions().await {
            s1.sessions.set(sessions);
        }
        loop {
            gloo_timers::future::TimeoutFuture::new(10_000).await;
            if let Ok(sessions) = api::load_sessions().await {
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
            }
        }
    });

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
