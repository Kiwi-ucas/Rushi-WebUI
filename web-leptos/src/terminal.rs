//! M9: terminal tab — xterm.js (vendored UMD, exposed as
//! `window.Terminal` by `vendor/xterm/xterm.js`) bridged to the
//! server-side PTY over the active session's WS connection.
//!
//! Outbound frames (sent via `ws::term_*`):
//!   term_open  {cols, rows}   spawn the shell in the session workdir
//!   term_input {data: base64}  raw keystrokes
//!   term_resize{cols, rows}   SIGWINCH the pty
//!   term_close                kill the pty's process group
//! Inbound frames (routed by `ws.rs`):
//!   term_out    {data: base64}  pty master output → xterm.write
//!   term_status {running: bool}  shell spawn / exit
//!
//! A fresh xterm instance is created per session (the panel's terminal
//! view is re-keyed by the active session) and torn down with
//! `term_close` on unmount, so no PTY outlives its tab / session.

use std::cell::RefCell;
use std::collections::HashMap;

use js_sys;
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use crate::model::AppState;
use crate::ws;

thread_local! {
    /// M11: the per-terminal xterm `onData`/`onResize` Closures, kept
    /// alive for each terminal's lifetime (keyed by terminal tab id;
    /// xterm holds them as JS properties — dropping the Rust `Closure`
    /// would detach the callback). Entries are removed on unmount.
    static TERM_HOOKS: RefCell<Option<
        HashMap<u32, (Closure<dyn FnMut(JsValue)>, Closure<dyn FnMut(JsValue)>)>,
    >> = const { RefCell::new(None) };
}

// ── xterm.js interop (plain Reflect calls on the UMD global) ─────────

/// The `window.Terminal` constructor, downcast to a `js_sys::Function`
/// (this js-sys build's `Reflect::apply` takes a `&Function`, not a
/// `&JsValue`).
fn global_terminal() -> Result<js_sys::Function, String> {
    let g = js_sys::global();
    let ctor = js_sys::Reflect::get(&g, &JsValue::from_str("Terminal"))
        .map_err(|_| "window.Terminal missing (vendor/xterm.js not loaded?)".to_string())?;
    if !ctor.is_function() {
        return Err("window.Terminal is not a function".to_string());
    }
    Ok(ctor.unchecked_into())
}

/// Call a 0-arg method on `obj` (as a method, `this` = obj).
fn call0(obj: &JsValue, name: &str) -> Result<JsValue, String> {
    let f = js_sys::Reflect::get(obj, &JsValue::from_str(name))
        .map_err(|e| e.as_string().unwrap_or_else(|| "property missing".to_string()))?;
    let f = f.unchecked_ref::<js_sys::Function>();
    let args = js_sys::Array::new();
    js_sys::Reflect::apply(f, obj, &args)
        .map_err(|e| e.as_string().unwrap_or_else(|| "call failed".to_string()))
}

/// Call a 1-arg method on `obj`.
fn call1(obj: &JsValue, name: &str, arg: &JsValue) -> Result<JsValue, String> {
    let f = js_sys::Reflect::get(obj, &JsValue::from_str(name))
        .map_err(|e| e.as_string().unwrap_or_else(|| "property missing".to_string()))?;
    let f = f.unchecked_ref::<js_sys::Function>();
    let args = js_sys::Array::new();
    args.set(0, arg.clone());
    js_sys::Reflect::apply(f, obj, &args)
        .map_err(|e| e.as_string().unwrap_or_else(|| "call failed".to_string()))
}

/// Create an xterm instance, open it on `host`, and attach the `onData`
/// / `onResize` callbacks (by reference — the caller keeps the owned
/// Closures alive in `TERM_HOOKS` for the terminal's lifetime).
/// M11: subscribe a callback to an xterm emitter via its public
/// getter (`term.onData(cb)` / `term.onResize(cb)`). xterm's `onData`
/// and `onResize` are getter-only prototype accessors that return the
/// emitter's subscribe function, so `Reflect::set` (own-property
/// assignment) cannot shadow them — the subscribe call is required.
fn subscribe_event(term: &JsValue, prop: &str, cb: &Closure<dyn FnMut(JsValue)>) -> Result<(), String> {
    let fnv = js_sys::Reflect::get(term, &JsValue::from_str(prop))
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("get {prop} failed").to_string()))?;
    let sub: js_sys::Function = fnv.unchecked_into();
    let args = js_sys::Array::new();
    args.set(0, cb.as_js_value().clone());
    let _disp = js_sys::Reflect::apply(&sub, term, &args)
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("subscribe {prop} failed").to_string()))?;
    Ok(())
}

fn create_xterm(
    host: &JsValue,
    on_data: &Closure<dyn FnMut(JsValue)>,
    on_resize: &Closure<dyn FnMut(JsValue)>,
) -> Result<JsValue, String> {
    let ctor = global_terminal()?;
    let (fg, bg) = current_theme_colors();

    let opts = js_sys::Object::new();
    let opts_v = JsValue::from(&opts);
    let _ = js_sys::Reflect::set(&opts_v, &JsValue::from_str("convertEol"), &JsValue::from(true));
    let _ = js_sys::Reflect::set(&opts_v, &JsValue::from_str("cursorBlink"), &JsValue::from(true));
    let _ = js_sys::Reflect::set(&opts_v, &JsValue::from_str("scrollback"), &JsValue::from(5000u32));
    let _ = js_sys::Reflect::set(&opts_v, &JsValue::from_str("fontSize"), &JsValue::from(12.0f64));
    let _ = js_sys::Reflect::set(
        &opts_v,
        &JsValue::from_str("fontFamily"),
        &JsValue::from_str("ui-monospace, SFMono-Regular, Menlo, monospace"),
    );
    let theme = js_sys::Object::new();
    let theme_v = JsValue::from(&theme);
    // M11: an OPAQUE background matched to rushi's --code-bg — the
    // vendored xterm.css hard-codes the viewport to black, so the
    // background must be painted by xterm itself (style.css overrides
    // the viewport to transparent on top of this).
    let _ = js_sys::Reflect::set(&theme_v, &JsValue::from_str("background"), &JsValue::from_str(bg));
    let _ = js_sys::Reflect::set(&theme_v, &JsValue::from_str("foreground"), &JsValue::from_str(fg));
    let _ = js_sys::Reflect::set(&opts_v, &JsValue::from_str("theme"), &theme_v);

    let ctor_args = js_sys::Array::new();
    ctor_args.set(0, opts_v.clone());
    // M11 fix: `Terminal` is an ES class constructor - `Reflect::apply`
    // (a plain function call) throws "Class constructor cannot be
    // invoked as a function". `Reflect::construct` is the `new`
    // equivalent, so use it here.
    let term = js_sys::Reflect::construct(&ctor, &ctor_args)
        .map_err(|e| e.as_string().unwrap_or_else(|| "Terminal() threw".to_string()))?;

    call1(&term, "open", host)?;

    // Test affordance (e2e probes): expose the xterm instance on its host
    // element so a CDP script can drive the real input pipeline via
    // `host.__term.input(data)` (xterm's public `input()` -> onData ->
    // term_input -> pty -> term_out -> xterm), which a synthetic DOM
    // InputEvent does not reliably trigger.
    let _ = js_sys::Reflect::set(host, &JsValue::from_str("__term"), &term);

    // Attach the callbacks. xterm exposes `onData` / `onResize` as
    // GETTER-ONLY accessors on the prototype (they return the emitter's
    // subscribe function), so a plain own-property assignment
    // (`Reflect::set`) is silently ignored — the callbacks must be
    // SUBSCRIBED by calling the emitter: `term.onData(cb)`,
    // `term.onResize(cb)`. The Closures are kept alive by TERM_HOOKS.
    subscribe_event(&term, "onData", on_data)?;
    subscribe_event(&term, "onResize", on_resize)?;

    Ok(term)
}

/// Feed decoded pty bytes into the terminal (xterm accepts a
/// Uint8Array). js-sys has no `&[u8] -> Uint8Array` constructor, so
/// allocate a zero-filled typed array of the right length and copy.
fn write(term: &JsValue, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let u8 = js_sys::Uint8Array::new(&JsValue::from_f64(data.len() as f64));
    u8.copy_from(data);
    let _ = call1(term, "write", &JsValue::from(&u8));
}

/// Dev diagnostic: log a line to the browser console (visible to the CDP
/// probes and to a human debugging a broken mount).
fn dbg_term(msg: &str) {
    let s = serde_json::to_string(msg).unwrap_or_default();
    let _ = js_sys::eval(&format!("console.log({s})"));
}

fn focus(term: &JsValue) {
    let _ = call0(term, "focus");
}

fn dispose(term: &JsValue) {
    let _ = call0(term, "dispose");
}

fn dims(term: &JsValue) -> (u32, u32) {
    let cols = js_sys::Reflect::get(term, &JsValue::from_str("cols"))
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(80.0);
    let rows = js_sys::Reflect::get(term, &JsValue::from_str("rows"))
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(24.0);
    ((cols.max(4.0)) as u32, (rows.max(2.0)) as u32)
}

/// M11: xterm.js colors matched to the active rushi theme. The
/// background is the opaque `--code-bg` equivalent per theme (light
/// `#f1eee3` / dark `#131009`); the foreground is the matching text
/// tone. The terminal well (`#term-host`) uses the same `--code-bg`,
/// so the xterm face and the panel well read as one surface.
fn current_theme_colors() -> (&'static str, &'static str) {
    let theme = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .and_then(|r| r.get_attribute("data-theme"));
    match theme.as_deref() {
        Some("light") => ("#3a3733", "#f1eee3"),
        _ => ("#e6e2d8", "#131009"),
    }
}

// ── The terminal view (one xterm instance per terminal tab) ──────────

/// Mount one xterm instance (for terminal tab `term_id`) into a fresh
/// host div, wire it to the WS terminal frames for that id, and tear it
/// down on unmount — unmounting closes just this pty, so closing a tab
/// kills that shell while the other tabs' shells keep running.
///
/// `term_open` is sent once the session socket is OPEN (it is dropped
/// by `ws::send_command` otherwise) — the connection may still be
/// mid-handshake when this mounts, so retry briefly.
#[component]
fn TermMount(state: AppState, term_id: u32, restart: RwSignal<u32>) -> impl IntoView {
    let term_state = state.term_state;

    // The host div is captured by the `use:` directive below; the
    // signal drives the mount Effect.
    let term_js = RwSignal::new(None::<JsValue>);
    let host_sig = RwSignal::new(None::<JsValue>);

    let host_sig_eff = host_sig;
    let term_js_eff = term_js;
    let state_open = state;
    let term_state_open = term_state;
    Effect::new(move || {
        let Some(host) = host_sig_eff.get() else {
            dbg_term(&format!("term {term_id}: effect ran before host attach"));
            return;
        };
        if term_js_eff.get().is_some() {
            return; // already mounted
        }
        dbg_term(&format!("term {term_id}: creating xterm"));
        let on_data = Closure::<dyn FnMut(JsValue)>::wrap(Box::new(move |data: JsValue| {
            // Pass the raw keystroke string; `ws::term_input` base64-encodes
            // it exactly once (the server does a single b64decode).
            if let Some(s) = data.as_string() {
                ws::term_input(term_id, s.as_str());
            }
        }) as Box<dyn FnMut(JsValue)>);
        let on_resize = Closure::<dyn FnMut(JsValue)>::wrap(Box::new(move |info: JsValue| {
            let cols = js_sys::Reflect::get(&info, &JsValue::from_str("cols"))
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(80.0) as u32;
            let rows = js_sys::Reflect::get(&info, &JsValue::from_str("rows"))
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(24.0) as u32;
            // Ignore degenerate resizes (a hidden pane reports 0×0); the
            // pty keeps its last real size until it is shown again.
            if cols >= 2 && rows >= 2 {
                ws::term_resize(term_id, cols, rows);
            }
        }) as Box<dyn FnMut(JsValue)>);
        match create_xterm(&host, &on_data, &on_resize) {
            Ok(term) => {
                dbg_term(&format!("term {term_id}: xterm created"));
                let el: web_sys::Element = host.unchecked_into();
                let _ = el.set_attribute("data-term", "ok");
                // Keep the owned Closures alive for the terminal's
                // lifetime (xterm holds them as JS properties; the
                // Rust Closures must outlive the instance or the
                // callbacks detach).
                TERM_HOOKS.with(|c| {
                    c.borrow_mut()
                        .get_or_insert_with(HashMap::new)
                        .insert(term_id, (on_data, on_resize));
                });
                term_js_eff.set(Some(term.clone()));
                let out_term = term.clone();
                // Route this terminal's pty output (term_out frames
                // carrying its id) into this xterm instance; the sink
                // is cleared on unmount.
                ws::set_term_writer(term_id, move |bytes: Vec<u8>| {
                    write(&out_term, &bytes);
                });
                focus(&term);
                // term_open once the socket is up (retry ~10 s).
                let tsig = term_js_eff;
                let ts_map = term_state_open;
                leptos::task::spawn_local(async move {
                    let mut opened = false;
                    for _ in 0..40 {
                        if ws::is_open() {
                            opened = true;
                            break;
                        }
                        gloo_timers::future::TimeoutFuture::new(250).await;
                    }
                    let t = tsig.get().unwrap_or(JsValue::UNDEFINED);
                    let (c, r) = dims(&t);
                    if opened {
                        ws::term_open(term_id, c, r);
                    } else {
                        ts_map.update(|m| {
                            if let Some(ts) = m.get_mut(&term_id) {
                                ts.error = Some("terminal: WebSocket not connected".to_string());
                            }
                        });
                    }
                });
            }
            Err(e) => {
                dbg_term(&format!("term {term_id}: xterm create FAILED: {e}"));
                let el: web_sys::Element = host.unchecked_into();
                let _ = el.set_attribute("data-term-err", &e);
                state_open.term_state.update(|m| {
                    if let Some(ts) = m.get_mut(&term_id) {
                        ts.error = Some(format!("terminal: {e}"));
                    }
                });
            }
        }
    });

    // "restart shell" (from the exit overlay): re-issue term_open for
    // this id — the server closes any dead entry and spawns a fresh pty
    // under the same id.
    let restart_sig = restart;
    let term_js_restart = term_js;
    Effect::new(move || {
        let _ = restart_sig.get();
        if restart_sig.get() == 0 {
            return;
        }
        if let Some(t) = term_js_restart.get() {
            let (c, r) = dims(&t);
            ws::term_open(term_id, c, r);
        }
    });

    // Unmount: detach this terminal's output sink, kill just this pty,
    // drop its callback keeps, and dispose xterm.
    let term_js_cleanup = term_js;
    on_cleanup(move || {
        ws::clear_term_writer(term_id);
        ws::term_close(term_id);
        TERM_HOOKS.with(|c| {
            if let Some(m) = c.borrow_mut().as_mut() {
                m.remove(&term_id);
            }
        });
        if let Some(t) = term_js_cleanup.get() {
            dispose(&t);
        }
    });

    // `use:` directives need the handler as a bare identifier, so the
    // mount hook is a named closure that stashes the host element.
    let host_sig_attach = host_sig;
    let attach = move |el: web_sys::Element| {
        host_sig_attach.set(Some(el.unchecked_into()));
    };

    view! {
        <div
            class=move || {
                let mut c = String::from("term-host");
                if term_js.get().is_none() {
                    c.push_str(" term-idle");
                }
                c
            }
            use:attach
        />
    }
}

/// M9: compact working-directory hint for the terminal status bar —
/// the last path component of the active session's cwd ("no project"
/// when the session has no `.cwd` marker).
fn term_cwd_label(sessions: RwSignal<Vec<crate::model::SessionInfo>>, active: RwSignal<Option<String>>) -> String {
    let Some(name) = active.get() else {
        return String::new();
    };
    match sessions.get().iter().find(|s| s.name == name) {
        Some(s) => s
            .cwd
            .clone()
            .map(|c| c.rsplit('/').next().unwrap_or(c.as_str()).to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// M11: the terminal tab view for terminal tab `term_id` — a status
/// bar (running lamp + tab label + cwd hint), the xterm host, and a
/// "shell exited — restart" overlay. All open terminal tabs stay
/// mounted (their shells keep running); `term_state[term_id]` drives
/// this tab's lamp and overlay.
#[component]
pub fn TerminalView(state: AppState, term_id: u32) -> impl IntoView {
    let active = state.active_session;
    let sessions = state.sessions;
    let term_state = state.term_state;
    let restart = RwSignal::new(0u32);

    view! {
        <div class="term-wrap">
            <div class="term-statusbar">
                <span
                    class=move || {
                        let snap = term_state.get();
                        let ts = snap.get(&term_id);
                        let running = ts.map(|t| t.running).unwrap_or(false);
                        let started = ts.map(|t| t.started).unwrap_or(false);
                        match (running, started) {
                            (true, _) => "tsc-dot running".to_string(),
                            (_, true) => "tsc-dot dead".to_string(),
                            _ => "tsc-dot".to_string(),
                        }
                    }
                />
                <span class="tsc-label">
                    { move || {
                        let label = state.rp_tabs
                            .get()
                            .iter()
                            .find(|t| t.id == term_id)
                            .map(|t| t.label.clone())
                            .unwrap_or_default();
                        let sess = active.get().unwrap_or_default();
                        format!("{label} · {sess}")
                    } }
                </span>
                <span class="tsc-hint">{ move || term_cwd_label(sessions, active) }</span>
            </div>
            { TermMount(TermMountProps {
                state,
                term_id,
                restart: restart.clone(),
            }) }
            <Show
                when=move || {
                    let snap = term_state.get();
                    let ts = snap.get(&term_id);
                    let has_err = ts.and_then(|t| t.error.as_ref()).is_some();
                    let exited = ts.map(|t| t.started && !t.running).unwrap_or(false);
                    has_err || exited
                }
                fallback=|| ()
            >
                <div class="term-exit-overlay">
                    <div class="term-exit-card">
                        <span class="term-exit-text">
                            { move || {
                                let snap = term_state.get();
                                let ts = snap.get(&term_id);
                                match ts.and_then(|t| t.error.clone()) {
                                    Some(e) => e,
                                    None => "shell exited".to_string(),
                                }
                            } }
                        </span>
                        <span class="term-exit-hint">
                            { "the shell stopped — restart to open a new one" }
                        </span>
                        <button
                            class="term-exit-btn"
                            on:click=move |_| {
                                state.term_state.update(|m| {
                                    if let Some(ts) = m.get_mut(&term_id) {
                                        ts.started = false;
                                        ts.error = None;
                                    }
                                });
                                restart.update(|r| *r += 1);
                            }
                        >
                            { "restart shell" }
                        </button>
                    </div>
                </div>
            </Show>
        </div>
    }
}
