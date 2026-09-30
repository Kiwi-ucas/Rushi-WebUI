//! Live event stream: one WebSocket per session (port of JS connectWS/sendWS).
//! Built on web-sys `WebSocket` because gloo-net 0.6 has no `ws` feature.

use std::cell::RefCell;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use serde_json::Value;
use web_sys::{CloseEvent, MessageEvent, WebSocket};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::markdown::normalize_event;
use crate::model::AppState;

thread_local! {
    /// M11: live sinks for terminal output, keyed by terminal id (one
    /// xterm writer per open terminal tab). `TermMount` registers its
    /// sink on mount and clears it on unmount; `term_out` frames carry
    /// the terminal id and are routed to the matching sink.
    static TERM_WRITE: RefCell<Option<std::collections::HashMap<u32, Box<dyn FnMut(Vec<u8>)>>>> =
        const { RefCell::new(None) };
}

/// M11: register the xterm writer for terminal `id` (mount of one tab).
/// Re-registering the same id replaces the previous sink.
pub fn set_term_writer(id: u32, f: impl FnMut(Vec<u8>) + 'static) {
    TERM_WRITE.with(|c| c.borrow_mut().get_or_insert_with(std::collections::HashMap::new).insert(id, Box::new(f)));
}

/// M11: clear the sink for terminal `id` (tab unmount / session switch).
pub fn clear_term_writer(id: u32) {
    TERM_WRITE.with(|c| {
        if let Some(m) = c.borrow_mut().as_mut() {
            m.remove(&id);
        }
    });
}

/// M11: clear every terminal sink (session switch tears down all ptys).
pub fn clear_all_term_writers() {
    TERM_WRITE.with(|c| {
        if let Some(m) = c.borrow_mut().as_mut() {
            m.clear();
        }
    });
}

/// M11: route decoded pty output to the sink registered for `id`
/// (a no-op when that terminal's view is not mounted).
fn deliver_term_out(id: u32, bytes: Vec<u8>) {
    TERM_WRITE.with(|c| {
        if let Some(m) = c.borrow_mut().as_mut() {
            if let Some(w) = m.get_mut(&id) {
                w(bytes);
            }
        }
    });
}

/// M9: base64-encode a keystroke string for a `term_input` frame.
pub fn b64_encode(s: &str) -> String {
    B64.encode(s.as_bytes())
}

thread_local! {
    /// The live socket for the active session; replaced (and closed) on
    /// session switch. Dropping the stored Closures deregisters handlers.
    static WS_LIVE: RefCell<Option<LiveSocket>> = const { RefCell::new(None) };

    /// v0.5.23: model_stream deltas coalesced into one update per
    /// animation frame. The server streams deltas at 60fps and a single
    /// 16ms poll cycle can read several of them, so one frame may see
    /// K WS messages. The pre-v0.5.23 per-message path ran a full
    /// Leptos notify + markdown re-parse + engine rAF step PER message,
    /// and K of those saturated the main thread during long
    /// generations (clicks starved; CSS kept animating). Now deltas
    /// accumulate here and a single rAF flush applies them.
    static PENDING_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
    static PENDING_REASONING: RefCell<String> = const { RefCell::new(String::new()) };
    /// A frame flush is already queued; `schedule_delta_flush` is a
    /// no-op until it fires and re-arms.
    static FLUSH_SCHEDULED: RefCell<bool> = const { RefCell::new(false) };
}

/// v0.5.23: queue `flush` for the next animation frame; at most one
/// flush per frame regardless of how many delta messages arrived.
fn schedule_delta_flush(flush: &Closure<dyn Fn()>) {
    let already = FLUSH_SCHEDULED.with(|f| {
        let mut f = f.borrow_mut();
        let s = *f;
        *f = true;
        s
    });
    if already {
        return;
    }
    if let Some(w) = web_sys::window() {
        let _ = w.request_animation_frame(flush.as_js_value().unchecked_ref::<js_sys::Function>());
    }
}

/// v0.5.23: discard coalesced deltas still waiting for the next frame
/// (final `assistant_message` / stream error / session switch — a
/// queued flush must not re-append text or revive the streaming card).
fn drop_pending_deltas() {
    PENDING_TEXT.with(|b| b.borrow_mut().clear());
    PENDING_REASONING.with(|b| b.borrow_mut().clear());
}

#[allow(dead_code)] // fields are never read; they keep the Closures alive
struct LiveSocket {
    socket: WebSocket,
    /// Keep the Closures alive for the socket's lifetime.
    on_msg: Closure<dyn FnMut(MessageEvent)>,
    on_close: Closure<dyn FnMut(CloseEvent)>,
    on_error: Closure<dyn FnMut()>,
}

pub fn is_open() -> bool {
    WS_LIVE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.socket.ready_state() == WebSocket::OPEN)
            .unwrap_or(false)
    })
}

/// Close (and drop) the current socket, if any.
pub fn close_current() {
    let _old = WS_LIVE.with(|cell| cell.borrow_mut().take());
}

/// Open the session's socket. The server first replays full history
/// (kind "history"), then streams each new events.jsonl line (kind
/// "event") and each live model delta (kind "model_stream"), plus
/// out-of-band error lines (kind "error").
/// v0.5.23: the per-frame flush of coalesced model_stream deltas, built
/// for `state`'s live-stream signals. Used by connect() and by the freeze
/// regression test, so both exercise the same coalescing path.
fn make_delta_flush(state: &AppState) -> Closure<dyn Fn()> {
    let live_text = state.live_text;
    let live_reasoning = state.live_reasoning;
    let streaming = state.streaming;
    Closure::wrap(Box::new(move || {
        FLUSH_SCHEDULED.with(|f| *f.borrow_mut() = false);
        let text = PENDING_TEXT.with(|b| std::mem::take(&mut *b.borrow_mut()));
        let reasoning = PENDING_REASONING.with(|b| std::mem::take(&mut *b.borrow_mut()));
        if text.is_empty() && reasoning.is_empty() {
            return;
        }
        if !text.is_empty() {
            live_text.update(|s| s.push_str(&text));
        }
        if !reasoning.is_empty() {
            live_reasoning.update(|s| s.push_str(&reasoning));
        }
        streaming.set(true);
        crate::pile::on_change();
    }) as Box<dyn Fn()>)
}

pub fn connect(state: &AppState, session: &str) {
    close_current();
    // A fresh connection owns a fresh live-stream state: no stale
    // streamed text, no phantom running tool cards.
    state.clear_live();
    // v0.5.17: reset truncated-history window state on (re)connect
    state.hist_oldest_line.set(0);
    state.hist_has_more.set(false);
    state.loading_earlier.set(false);
    // v0.5.21: per-connection "load earlier" bookkeeping resets too.
    state.hist_total_rounds.set(0);
    state.earlier_loaded.set(0);
    state.earlier_failed.set(false);
    // v0.5.23: drop coalesced deltas left over from the previous
    // connection; a queued flush then finds empty buffers and no-ops,
    // so it can never append into the new session's live card.
    drop_pending_deltas();
    // M11: a fresh connection owns fresh (no) terminals — the previous
    // socket's ptys were torn down with it. Reset every terminal's
    // liveness state and detach every xterm writer sink.
    state.term_state.set(std::collections::HashMap::new());
    clear_all_term_writers();

    let w = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let loc = w.location();
    let proto = match loc.protocol().ok().as_deref() {
        Some("https:") => "wss",
        _ => "ws",
    };
    let host = loc.host().ok().unwrap_or_default();
    let encoded: String = js_sys::encode_uri_component(session).into();
    let url = format!("{proto}://{host}/ws/sessions/{encoded}");

    let socket = match WebSocket::new(&url) {
        Ok(s) => s,
        Err(_) => {
            state.ws_status.set("error".to_string());
            return;
        }
    };
    state.ws_status.set("connecting".to_string());

    let events = state.events;
    let ws_status = state.ws_status;
    let ctx_used = state.ctx_used;
    let rounds_ctxk = state.rounds_ctxk;
    let loop_running = state.loop_running;
    let live_text = state.live_text;
    let live_reasoning = state.live_reasoning;
    let streaming = state.streaming;
    let tool_pending = state.tool_pending;
    let looping = state.looping_sessions;
    let done_unviewed = state.loop_done_unviewed;
    let settling = state.settling_card;
    // v0.5.30: "output" sidebar rank — bumped only when a loop
    // COMPLETES (the running=false branch of "loop_status").
    let output_rank = state.output_rank;
    // M11: per-terminal liveness (lamp + exit overlay), keyed by the
    // terminal tab id the server echoes back in term_status frames.
    let term_state = state.term_state;
    let session_name = session.to_string();
    let hist_oldest_line = state.hist_oldest_line;
    let hist_has_more = state.hist_has_more;
    let loading_earlier = state.loading_earlier;
    let hist_total_rounds = state.hist_total_rounds;
    let earlier_loaded = state.earlier_loaded;
    let earlier_failed = state.earlier_failed;
    let ev_gen = state.ev_gen;

    // v0.5.23: the per-frame flush of coalesced model_stream deltas
    // (see make_delta_flush). Created per connection — it captures THIS
    // socket's signals — and is kept alive inside on_msg's capture box
    // (see LiveSocket).
    let flush = make_delta_flush(state);

    let on_msg = {
        let events = events;
        let ctx_used = ctx_used;
        let rounds_ctxk = rounds_ctxk;
        let loop_running = loop_running;
        let live_text = live_text;
        let live_reasoning = live_reasoning;
        let streaming = streaming;
        let settling = settling;
        let tool_pending = tool_pending;
        let looping = looping;
        let done_unviewed = done_unviewed;
        let output_rank = output_rank;
        let session_name = session_name;
        let hist_oldest_line = hist_oldest_line;
        let hist_has_more = hist_has_more;
        let loading_earlier = loading_earlier;
        let hist_total_rounds = hist_total_rounds;
        let earlier_loaded = earlier_loaded;
        let earlier_failed = earlier_failed;
        let ev_gen = ev_gen;
        let term_state = term_state;
        let flush = flush;
        Closure::wrap(Box::new(move |e: MessageEvent| {
            let data = match e.data().as_string() {
                Some(d) => d,
                None => return,
            };
            let Ok(items) = serde_json::from_str::<Vec<Value>>(&data) else {
                return;
            };
            for item in items {
                let kind = item.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                match kind {
                    "history" => {
                        let evs = item
                            .get("events")
                            .cloned()
                            .and_then(|v| serde_json::from_value::<Vec<Value>>(v).ok())
                            .unwrap_or_default();
                        let normed: Vec<Value> =
                            evs.into_iter().map(|mut v| normalize_event(&mut v)).collect();
                        let (ctx, ctxk) = rebuild_ctx_bookkeeping(&normed);
                        ctx_used.set(ctx);
                        rounds_ctxk.set(ctxk);
                        tool_pending.set(rebuild_tool_pending(&normed));
                        settling.set(false); // v0.5.15: a history replay never settles
                        // v0.5.17: truncated-history window state.
                        let oldest = item.get("oldest_line").and_then(|v| v.as_u64()).unwrap_or(1);
                        let has_more = item.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false);
                        let total_rounds = item.get("total_rounds").and_then(|v| v.as_u64()).unwrap_or(0);
                        hist_oldest_line.set(oldest);
                        hist_has_more.set(has_more);
                        hist_total_rounds.set(total_rounds);
                        earlier_loaded.set(0);
                        earlier_failed.set(false);
                        loading_earlier.set(false);
                        events.set(normed);
                        // v0.5.33: this REPLACES the loaded list (fresh
                        // connection, or a reconnect after the window
                        // may have grown) — bump the generation so the
                        // transcript's keyed For re-keys and rebuilds.
                        ev_gen.update(|g| *g += 1);
                        // Drive the pile engine directly (the Transcript
                        // effect is a second, redundant trigger): park
                        // the view at the last message of the history.
                        crate::pile::on_history_loaded();
                    }
                    // v0.5.17: older page of events ("load earlier").
                    "history_page" => {
                        let evs = item
                            .get("events")
                            .cloned()
                            .and_then(|v| serde_json::from_value::<Vec<Value>>(v).ok())
                            .unwrap_or_default();
                        let new_count = evs.len();
                        // v0.5.33: window bookkeeping applies to the
                        // EMPTY terminal page too (has_more=false must
                        // reach the pill even when 0 events came back).
                        let oldest = item.get("oldest_line").and_then(|v| v.as_u64()).unwrap_or(1);
                        let has_more = item.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false);
                        let total_rounds = item.get("total_rounds").and_then(|v| v.as_u64()).unwrap_or(0);
                        hist_oldest_line.set(oldest);
                        hist_has_more.set(has_more);
                        hist_total_rounds.set(total_rounds);
                        if new_count > 0 {
                            let normed: Vec<Value> =
                                evs.into_iter().map(|mut v| normalize_event(&mut v)).collect();
                            crate::pile::on_history_prepended();
                            let merged = {
                                let mut v = normed;
                                v.extend(events.get());
                                v
                            };
                            let (ctx, ctxk) = rebuild_ctx_bookkeeping(&merged);
                            ctx_used.set(ctx);
                            rounds_ctxk.set(ctxk);
                            tool_pending.set(rebuild_tool_pending(&merged));
                            earlier_loaded.update(|v| *v += new_count as u64);
                            events.set(merged);
                            // v0.5.33: prepend shifts every index — bump
                            // the generation so the keyed For re-keys
                            // and rebuilds all cards (a plain index key
                            // would retain the pre-prepended views and
                            // swallow this page).
                            ev_gen.update(|g| *g += 1);
                        }
                        earlier_failed.set(false);
                        loading_earlier.set(false);
                    }
                    "event" => {
                        let raw = item.get("data").cloned().unwrap_or(Value::Null);
                        let parsed = raw
                            .as_str()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok());
                        let ev = match parsed {
                            Some(mut v) => normalize_event(&mut v),
                            None => normalize_event(&mut serde_json::json!({
                                "type": "raw",
                                "value": raw,
                            })),
                        };
                        let t = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let content = ev
                            .get("content")
                            .and_then(|c| c.as_str())
                            .map(|s| s.to_string());
                        if t == "assistant_message" {
                            if let Some(n) = ev
                                .get("usage")
                                .and_then(|u| u.get("input_tokens"))
                                .and_then(|n| n.as_u64())
                            {
                                if n > 0 {
                                    ctx_used.set(n);
                                }
                            }
                        } else if t == "user_message" {
                            // v0.5.23: a new round starts; drop any
                            // coalesced deltas left over from the
                            // previous one (a queued flush must not
                            // re-append into the new round's card).
                            drop_pending_deltas();
                            // A user_message closes the previous round
                            // (legacy curRound bookkeeping). Record ctxK
                            // EXACTLY ONCE: for our own sends, do_send
                            // (ui.rs) already pushed it when it created
                            // the optimistic card, so this handler only
                            // records it when the event is NOT the echo
                            // of our own optimistic push.
                            let is_echo = content
                                .as_deref()
                                .is_some_and(|c| {
                                    events.with(|v| {
                                        v.iter().any(|e| {
                                            e.get("type").and_then(|x| x.as_str())
                                                == Some("user_message")
                                                && e.get("content").and_then(|x| x.as_str())
                                                    == Some(c)
                                                && e.get("optimistic")
                                                    .and_then(|o| o.as_bool())
                                                    == Some(true)
                                        })
                                    })
                                });
                            if !is_echo {
                                if !events.with(|v| v.is_empty()) {
                                    rounds_ctxk.update(|v| v.push(ctx_used.get()));
                                }
                            }
                        }
                        // Live-stream bookkeeping: the canonical card
                        // replaces the streamed one; tool ids move from
                        // pending to resolved as their results land.
                        match t {
                            "assistant_message" => {
                                // v0.5.15: if this event finalizes a live
                                // stream, mark it so the just-finalized
                                // card mounts with `.ev-settling` — it starts
                                // in the in-flight card's look (dark face +
                                // lifted relief) and glides to the settled
                                // light face. No color step, no flicker.
                                if streaming.get() {
                                    settling.set(true);
                                }
                                // v0.5.23: this event owns the final text;
                                // drop any coalesced deltas still queued so
                                // the pending rAF flush can't re-append them.
                                drop_pending_deltas();
                                live_text.set(String::new());
                                live_reasoning.set(String::new());
                                streaming.set(false);
                            }
                            "error" => {
                                // v0.5.23: same guard as assistant_message.
                                drop_pending_deltas();
                                live_text.set(String::new());
                                live_reasoning.set(String::new());
                                streaming.set(false);
                                settling.set(false);
                            }
                            "tool_call" => {
                                if let Some(cid) = ev.get("id").and_then(|i| i.as_str()).map(String::from) {
                                    tool_pending.update(|v| {
                                        if !v.iter().any(|p| p == &cid) {
                                            v.push(cid);
                                        }
                                    });
                                }
                            }
                            "tool_result" => {
                                if let Some(cid) = ev.get("id").and_then(|i| i.as_str()).map(String::from) {
                                    tool_pending.update(|v| v.retain(|p| p != &cid));
                                }
                            }
                            _ => {}
                        }
                        events.update(move |old| {
                            // Replace our own optimistic card with the
                            // canonical server user_message echo (and
                            // only user_messages) instead of pushing a
                            // duplicate render of the same message.
                            if ev.get("type").and_then(|v| v.as_str()) == Some("user_message") {
                                if let Some(c) = content.clone() {
                                    if let Some(pos) = old.iter().rposition(|e| {
                                        e.get("type").and_then(|x| x.as_str())
                                            == Some("user_message")
                                            && e.get("content").and_then(|x| x.as_str())
                                                == Some(c.as_str())
                                            && e.get("optimistic")
                                                .and_then(|o| o.as_bool())
                                                == Some(true)
                                    }) {
                                        old.remove(pos);
                                    }
                                }
                            }
                            old.push(ev);
                        });
                        crate::pile::on_change();
                    }
                    "error" => {
                        let message = item
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error");
                        let ev = serde_json::json!({
                            "type": "error",
                            "ts": crate::timeutil::now_iso(),
                            "message": message,
                        });
                        events.update(|old| old.push(ev));
                        // v0.5.21: an error frame can be the reply to a
                        // load_earlier command (e.g. "load_earlier failed: …").
                        // Without this the in-flight flag would stick true
                        // and the pill would stay disabled forever.
                        if loading_earlier.get() {
                            loading_earlier.set(false);
                            earlier_failed.set(true);
                        }
                        crate::pile::on_change();
                    }
                    "loops" => {
                        // v0.5.13: connect-time snapshot of the
                        // server-side running-loop set — resync the
                        // sidebar lamps on (re)connect.
                        let mut set: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        if let Some(a) = item.get("data").and_then(|v| v.as_array()) {
                            for v in a {
                                if let Some(s) = v.as_str() {
                                    set.insert(s.to_string());
                                }
                            }
                        }
                        *looping.write() = set;
                    }
                    "loop_status" => {
                        let running = item.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
                        let exit = item.get("exit").and_then(|v| v.as_i64());
                        let stopped = item.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false);
                        // The server's waiter attaches the tail of the
                        // loop's stderr when the death is abnormal, so
                        // the card shows *why* the loop died instead of
                        // just pointing at a file.
                        let detail = item.get("detail").and_then(|v| v.as_str()).map(String::from);
                        // v0.5.13: the frame carries its session — the
                        // server forwards every session's loop events to
                        // every client so each sidebar can light lamps
                        // for sessions that are not open. Frames for
                        // other sessions only touch the lamp sets; the
                        // global flag and the error card apply to this
                        // socket's session only.
                        let sname = item
                            .get("session")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        {
                            let mut ls = looping.write();
                            let mut du = done_unviewed.write();
                            if running {
                                ls.insert(sname.clone());
                                du.remove(&sname);
                            } else {
                                ls.remove(&sname);
                                du.insert(sname.clone());
                            }
                        }
                        // v0.5.30: "output" sidebar rank — the ONLY thing
                        // that moves the by-last-output order. Bumped
                        // here, at loop COMPLETION (running=false), so
                        // concurrent streaming sessions do not reshuffle
                        // the sidebar card-per-card; a finished loop
                        // jumps to the top.
                        if !running {
                            let now = js_sys::Date::now() / 1000.0;
                            output_rank.update(|rank| {
                                rank.insert(sname.clone(), now);
                            });
                        }
                        if sname == session_name {
                            // v0.5.50: the loop we started from a send is
                            // the "watch this round" signal itself — the
                            // engine re-asserts the follow on this edge
                            // (see pile::on_loop_start).
                            let was_running = loop_running.get_untracked();
                            loop_running.set(running);
                            if running && !was_running {
                                crate::pile::on_loop_start();
                            }
                            // An unexpected death (non-zero exit or
                            // killed by a signal, not our stop command)
                            // gets a card so the user can see the loop
                            // died and where to look.
                            let unexpected = !running
                                && !stopped
                                && match exit {
                                    Some(c) => c != 0,
                                    None => true,
                                };
                            if unexpected {
                                // The loop died mid-model-call: drop any
                                // half- streamed text so a stale
                                // "generating" card does not sit next to
                                // the error card.
                                live_text.set(String::new());
                                live_reasoning.set(String::new());
                                streaming.set(false);
                                let exit_desc = match exit {
                                    Some(c) => format!("exit {c}"),
                                    None => "killed by signal".to_string(),
                                };
                                let message = match detail {
                                    Some(d) if !d.is_empty() => format!(
                                        "loop stopped unexpectedly ({exit_desc}):\n{d}"
                                    ),
                                    _ => format!(
                                        "loop stopped unexpectedly ({exit_desc}); check loop.stderr in the session folder"
                                    ),
                                };
                                let ev = serde_json::json!({
                                    "type": "error",
                                    "ts": crate::timeutil::now_iso(),
                                    "message": message,
                                });
                                events.update(|old| old.push(ev));
                                crate::pile::on_change();
                            }
                        }
                    }
                    "model_stream" => {
                        // Live delta from the `.model-stream` side
                        // channel: the server inlines the raw delta JSON
                        // line (ModelDelta: text / reasoning /
                        // tool_call_delta / done).
                        let raw = item.get("data").cloned().unwrap_or(Value::Null);
                        let dline = match raw {
                            Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
                            v => v,
                        };
                        let dkind = dline.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                        let delta = dline.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                        // Only text/reasoning deltas have a live target.
                        // tool_call_delta and done carry no card of
                        // their own; the final events do the work.
                        if !delta.is_empty() {
                            // v0.5.23: coalesce into the per-frame buffer
                            // instead of a reactive update per message
                            // (the 60fps stream used to saturate the main
                            // thread; one rAF flush applies all pending
                            // deltas).
                            match dkind {
                                "reasoning" => {
                                    PENDING_REASONING.with(|b| b.borrow_mut().push_str(delta));
                                }
                                _ => {
                                    PENDING_TEXT.with(|b| b.borrow_mut().push_str(delta));
                                }
                            }
                            // At most one flush per animation frame.
                            schedule_delta_flush(&flush);
                        }
                    }
                    // M11: pty master output (base64) → the xterm writer
                    // registered for this terminal id (a no-op when that
                    // terminal's view is not mounted).
                    "term_out" => {
                        let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        if let Some(data) = item.get("data").and_then(|v| v.as_str()) {
                            if let Ok(bytes) = B64.decode(data) {
                                deliver_term_out(id, bytes);
                            }
                        }
                    }
                    // M11: shell spawn/close status, keyed by terminal id
                    // — drives that tab's running lamp and exit overlay.
                    "term_status" => {
                        let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let running = item.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
                        let error = item.get("error").and_then(|v| v.as_str()).map(String::from);
                        term_state.update(|m| {
                            let ts = m.entry(id).or_default();
                            ts.running = running;
                            if running {
                                ts.started = true;
                            }
                            ts.error = error;
                        });
                    }
                    _ => {}
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>)
    };

    let on_close = {
        let ws_status = ws_status;
        let st = *state;
        // `session_name` was moved into the on_msg closure above, so
        // build this socket's reconnect name directly from the `&str`
        // param (still owned-scoped here, not captured by the Closure).
        let sname = session.to_string();
        Closure::wrap(
            Box::new(move |_e: CloseEvent| {
                ws_status.set("disconnected".to_string());
                // v0.5.21: auto-reconnect. If this socket still owns the
                // active session, re-establish it after 1s. Guards: the
                // session-name check stops a superseded socket (user
                // already switched sessions) from resurrecting the old
                // one; the is_open() check avoids double-connecting while
                // a fresh socket is mid-handshake. Re-checked AFTER the
                // sleep so a session switch during the delay wins.
                if st.active_session.get().as_deref() == Some(sname.as_str()) && !is_open() {
                    // `st` is Copy; clone the String so the outer
                    // closure stays FnMut (it must outlive one close).
                    let st2 = st;
                    let sname2 = sname.clone();
                    spawn_local(async move {
                        gloo_timers::future::TimeoutFuture::new(1000).await;
                        if st2.active_session.get().as_deref() == Some(sname2.as_str()) && !is_open() {
                            connect(&st2, &sname2);
                        }
                    });
                }
            }) as Box<dyn FnMut(CloseEvent)>,
        )
    };

    let on_error = {
        let ws_status = ws_status;
        Closure::wrap(
            Box::new(move || {
                ws_status.set("error".to_string());
            }) as Box<dyn FnMut()>,
        )
    };

    // `as_js_value().unchecked_ref` is safe here: each stored
    // Closure is a JS-callable function of the right arity for its
    // slot, and LiveSocket keeps it alive for the socket's lifetime.
    let _ = socket.set_onmessage(Some(
        on_msg.as_js_value().unchecked_ref::<js_sys::Function>(),
    ));
    let _ = socket.set_onclose(Some(
        on_close.as_js_value().unchecked_ref::<js_sys::Function>(),
    ));
    let _ = socket.set_onerror(Some(
        on_error.as_js_value().unchecked_ref::<js_sys::Function>(),
    ));

    WS_LIVE.with(|cell| {
        cell.borrow_mut().replace(LiveSocket {
            socket,
            on_msg,
            on_close,
            on_error,
        });
    });
}

/// Send an in-band command to the server (port of JS sendWS): the
/// protocol sends single-item JSON arrays.
pub fn send_command(_session: &str, command: &Value) {
    let payload = serde_json::to_string(&vec![command]).unwrap_or_default();
    WS_LIVE.with(|cell| {
        let s = cell.borrow();
        if let Some(s) = s.as_ref() {
            let _ = s.socket.send_with_str(&payload);
        }
    });
}

/// v0.5.17: request the next OLDER page of events ("load earlier").
/// Pages backwards from the oldest loaded line; the server answers
/// with a `history_page` frame, handled in the on_msg closure above.
pub fn load_earlier(state: &AppState) {
    let active = match state.active_session.get() {
        Some(s) => s,
        None => return,
    };
    if state.loading_earlier.get() || !state.hist_has_more.get() {
        return;
    }
    // v0.5.37: capture the reader's exact viewport position NOW (before
    // the pill relabels to "loading…"), so the prepend can restore the
    // screen to the spot the user was actually reading.
    crate::pile::capture_earlier_anchor();
    state.loading_earlier.set(true);
    state.earlier_failed.set(false);
    let before = state.hist_oldest_line.get().max(1);
    send_command(
        &active,
        &serde_json::json!({ "kind": "load_earlier", "before_line": before, "limit": 200 }),
    );
    // v0.5.21: watchdog — if the history_page frame never arrives
    // (dead socket, old server that ignores the command, dropped
    // message), clear the in-flight flag so the pill un-sticks and
    // shows the retry hint instead of "loading…" forever.
    let st = *state;
    spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(5000).await;
        if st.loading_earlier.get() {
            st.loading_earlier.set(false);
            st.earlier_failed.set(true);
        }
    });
}

// ── M9/M11: terminal frames (the pty lives in the server's WS session) ─

/// M11: open a new PTY (id-tagged, so multiple terminals coexist on one
/// connection). The server spawns the user's shell in the session's
/// workdir and answers with a `term_status` frame carrying the same id.
pub fn term_open(id: u32, cols: u32, rows: u32) {
    send_command(
        "",
        &json!({ "kind": "term_open", "id": id, "cols": cols, "rows": rows }),
    );
}

/// M11: send raw keystrokes (already a UTF-8 string from xterm's
/// `onData`), tagged with the terminal id. Base64-escaped so arbitrary
/// bytes survive the WS text frame.
pub fn term_input(id: u32, data: &str) {
    send_command(
        "",
        &json!({ "kind": "term_input", "id": id, "data": b64_encode(data) }),
    );
}

/// M11: tell the server to resize a pty (xterm's `onResize` fires after a
/// window/panel resize), tagged with the terminal id.
pub fn term_resize(id: u32, cols: u32, rows: u32) {
    send_command(
        "",
        &json!({ "kind": "term_resize", "id": id, "cols": cols, "rows": rows }),
    );
}

/// M11: ask the server to kill ONE pty's process group (tab close /
/// session switch). The other open terminals on the connection are
/// untouched.
pub fn term_close(id: u32) {
    send_command("", &json!({ "kind": "term_close", "id": id }));
}

/// Rebuild the legacy ctx bookkeeping from a (partial or full) event
/// list: `ctx_used` is the last assistant input_tokens usage; each
/// user_message (except the very first event) closes the previous
/// round, recording `ctx_used` as that round's ctxK.
fn rebuild_ctx_bookkeeping(normed: &[Value]) -> (u64, Vec<u64>) {
    let mut ctx = 0u64;
    let mut ctxk: Vec<u64> = Vec::new();
    for (i, ev) in normed.iter().enumerate() {
        let t = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == "assistant_message" {
            // legacy updateCtxBar: only a truthy input_tokens count moves the bar.
            if let Some(n) = ev
                .get("usage")
                .and_then(|u| u.get("input_tokens"))
                .and_then(|n| n.as_u64())
            {
                if n > 0 {
                    ctx = n;
                }
            }
        } else if t == "user_message" && i > 0 {
            ctxk.push(ctx);
        }
    }
    (ctx, ctxk)
}

/// Rebuild the running tool-call set: tool_call ids with no matching
/// tool_result (only survives when a loop died mid-tool).
fn rebuild_tool_pending(normed: &[Value]) -> Vec<String> {
    let pending: Vec<String> = normed
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("tool_call"))
        .filter_map(|e| e.get("id").and_then(|i| i.as_str()).map(String::from))
        .collect();
    let done: Vec<String> = normed
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        .filter_map(|e| e.get("id").and_then(|i| i.as_str()).map(String::from))
        .collect();
    pending
        .into_iter()
        .filter(|id| !done.iter().any(|d| d == id))
        .collect()
}


// ── v0.5.23: freeze regression test (`?test=freeze`) ────────────────
//
// Repeatable regression test for the main-thread saturation that froze
// the UI during model streaming (the pre-v0.5.23 per-delta reactive
// update storm). Open the app with `?test=freeze` in the URL (add
// `&flood=raw` to emulate the PRE-fix behaviour as a negative
// control) and ~1.5 s after mount the test runs itself:
//
//   1. seeds the streaming card with a large markdown document,
//   2. drives the flood on a 16 ms timer chain (rAF never fires in
//      the headless shell the e2e runner uses; a real browser gets
//      the same cadence) — the default mode pushes each tick's delta
//      batch into the per-frame pending buffers and invokes the
//      production coalesced flush ONCE; the raw control fires K
//      separate one-delta timer tasks per tick (the pre-fix shape:
//      each WS message was its own task, so Leptos flushed between
//      deltas), while sampling main-thread responsiveness (tick
//      gaps + a 0 ms timer round-trip queued behind the flood),
//   3. restores the signals and writes a machine-readable verdict to
//      `window.__rushiFreezeResult`, the console, a visible
//      #rushi-freeze-badge, the <title>, and persistent
//      `<html data-freeze-verdict / -gap / -timer>` attributes (the
//      CDP runner e2e/freeze_regress.py reads those after the badge
//      auto-dismisses).
//
// PASS: max tick gap < 100 ms AND max timer round-trip < 150 ms while the
// document grows. The raw (pre-fix) control is EXPECTED to fail — that
// is how the test proves it discriminates. (Raw stops early, after
// FLOOD_EARLY_STOP_FRAMES ticks, once a threshold is exceeded.)

/// Deltas delivered per 16 ms tick. 60 (≈3.3 KB/tick) models a very
/// fast local burst: the raw control fires 60 separate one-delta
/// timer tasks per tick, and their full-cycle cost grows with the
/// document, so the bursts saturate the main thread (the pre-fix
/// freeze). The coalesced path pays ONE flush per tick no matter K,
/// so it stays smooth. (6 deltas were too mild to exceed the
/// thresholds even in raw mode — the test must discriminate, not
/// just run.)
const FLOOD_CHUNKS_PER_FRAME: u32 = 60;
/// ~1.5 s at 60 fps. Sized so the flood ends with a ~360 KB live
/// document: the coalesced path pays ONE re-parse/DOM cycle per tick
/// (stays under the 100 ms gate), while the raw control pays 60
/// re-parses per tick and exceeds the gates — from ~40 frames in
/// (see FLOOD_EARLY_STOP_FRAMES, which stops the raw run once the
/// failure is proven).
const FLOOD_FRAMES: u32 = 96;
/// Raw mode stops as soon as it has exceeded a threshold for this many
/// ticks (proven failure; no need to burn the full flood).
const FLOOD_EARLY_STOP_FRAMES: u32 = 24;
const FLOOD_SEED_BYTES: usize = 40_000;
const FLOOD_MAX_GAP_MS: f64 = 100.0;
const FLOOD_MAX_TIMER_MS: f64 = 150.0;
/// Driver tick cadence. The driver runs on a timer chain (NOT rAF):
/// the headless shell that executes this test has no display and fires
/// no animation frames, while timers run normally — a real browser
/// gets the same ~60 Hz cadence from the 16 ms timer.
const FLOOD_FRAME_MS: u32 = 16;

/// ~55 bytes: the size of one delta; a burst of 60 of these in one
/// 16 ms tick is the "fast local model" flood the UI must survive.
const FLOOD_CHUNK: &str = "delta — the model keeps writing, the UI must stay responsive. ";

/// Repeating block exercising every parser branch (heading, paragraph,
/// code fence, table, bullets) so the hot-tail re-parse does real work.
fn flood_seed() -> String {
    let block = "\n## Section\n\nRushi regression paragraph. The quick brown fox jumps over the lazy dog while the model streams a long answer into the transcript card.\n\n```\nfn sample() -> usize {\n    (0..64).map(|n| n.wrapping_mul(7)).sum()\n}\n```\n\n| round | tool | ctx |\n| --- | --- | --- |\n| 1 | read | 12k |\n| 2 | bash | 18k |\n\n- alpha\n- beta\n- gamma\n";
    let mut s = String::with_capacity(FLOOD_SEED_BYTES + block.len());
    while s.len() < FLOOD_SEED_BYTES {
        s.push_str(block);
    }
    s
}

/// Per-frame driver state for the flood (see `run_freeze_test`).
struct FloodState {
    frame: u32,
    last_ts: Option<f64>,
    max_gap_ms: f64,
    timer_max_ms: f64,
    /// Timer round-trip probes (one per tick). They must survive
    /// until they fire — a raw-mode burst queues them for a while —
    /// so they are kept for the test's lifetime and dropped all at
    /// once when the driver is done.
    pending: Vec<gloo_timers::callback::Timeout>,
    /// The next-tick timer (dropped when the test ends).
    tick: Option<gloo_timers::callback::Timeout>,
    /// The timer that starts the driver chain (kept alive until it
    /// fires; the driver then reschedules itself into `tick`).
    first: Option<gloo_timers::callback::Timeout>,
}

/// Write the verdict everywhere the test can be read from: the
/// console, `window.__rushiFreezeResult` (for scripts), and a visible
/// #rushi-freeze-badge.
fn flood_report(pass: Option<bool>, reason: &str, max_gap_ms: f64, timer_ms: f64, frames: u32, live_bytes: usize) {
    let label = match pass {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "SKIP",
    };
    let json = serde_json::json!({
        "pass": pass,
        "reason": reason,
        "max_gap_ms": max_gap_ms,
        "timer_ms": timer_ms,
        "frames": frames,
        "frames_expected": FLOOD_FRAMES,
        "live_bytes": live_bytes,
        "thresholds": { "max_gap_ms": FLOOD_MAX_GAP_MS, "timer_ms": FLOOD_MAX_TIMER_MS },
        "version": "0.5.23",
    });
    let js = format!(
        "window.__rushiFreezeResult = {}; console.log('[rushi-freeze] {} — {} (gap {gap:.0}ms, timer {clk:.0}ms, {frames}/{total} frames)');",
        json,
        label,
        reason,
        gap = max_gap_ms,
        clk = timer_ms,
        frames = frames,
        total = FLOOD_FRAMES
    );
    let _ = js_sys::eval(&js);
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        // Persistent verdict markers (the badge below auto-dismisses):
        // the tab title for humans, the html data attribute for
        // headless harnesses that dump the DOM after the badge is gone.
        let _ = doc.set_title(&format!("rushi freeze-test {label}"));
        if let Some(root) = doc.document_element() {
            let _ = root.set_attribute("data-freeze-verdict", label);
            let _ = root.set_attribute(
                "data-freeze-gap",
                &format!("{max_gap_ms:.1}"),
            );
            let _ = root.set_attribute(
                "data-freeze-timer",
                &format!("{timer_ms:.1}"),
            );
        }
        if let Ok(div) = doc.create_element("div") {
            div.set_id("rushi-freeze-badge");
            let color = if pass == Some(true) { "#4ade80" } else { "#f87171" };
            let _ = div.set_attribute(
                "style",
                &format!(
                    "position:fixed;top:12px;right:12px;z-index:99999;font:13px system-ui;padding:8px 12px;border-radius:8px;background:#1c1e21;color:{color};box-shadow:0 2px 12px rgba(0,0,0,.5)"
                ),
            );
            div.set_text_content(Some(&format!(
                "freeze-test {label}: {reason} — gap {gap:.0}ms / timer {clk:.0}ms",
                gap = max_gap_ms,
                clk = timer_ms,
            )));
            if let Some(body) = doc.body() {
                let _ = body.append_child(&div);
            }
            let div2 = div.clone();
            crate::pile::leak_timeout(gloo_timers::callback::Timeout::new(
                20_000,
                move || {
                    let _ = div2.remove();
                },
            ));
        }
    }
}

/// Run the freeze regression test. Auto-invoked when the URL carries
/// `?test=freeze` (see lib.rs); safe to call manually from the console.
pub fn run_freeze_test() {
    let Some(state) = crate::model::AppState::current_app_state() else {
        flood_report(None, "no AppState (call after mount)", 0.0, 0.0, 0, 0);
        return;
    };
    // rAF is throttled in background tabs; a verdict there is
    // meaningless.
    if js_sys::eval("document.visibilityState")
        .ok()
        .and_then(|v| v.as_string())
        .as_deref()
        == Some("hidden")
    {
        flood_report(
            None,
            "tab hidden (rAF throttled) — open in a visible tab",
            0.0,
            0.0,
            0,
            0,
        );
        return;
    }
    let raw_mode = web_sys::window()
        .and_then(|w| w.location().search().ok())
        .is_some_and(|q| q.contains("flood=raw"));

    // Snapshot the live-stream signals so the test can restore them.
    let old_text = state.live_text.get();
    let old_reasoning = state.live_reasoning.get();
    let old_streaming = state.streaming.get();

    PENDING_TEXT.with(|b| b.borrow_mut().clear());
    PENDING_REASONING.with(|b| b.borrow_mut().clear());
    FLUSH_SCHEDULED.with(|f| *f.borrow_mut() = false);

    // Responsiveness probe: every frame schedules a setTimeout(0) and
    // records how late it actually fired. A saturated main thread
    // (the pre-v0.5.23 situation) delays timer fires into the
    // hundreds of ms. (A synthetic el.click() handler would run
    // inline in the same task and measure nothing; a timer
    // round-trip does.)
    let timer_latency = std::rc::Rc::new(std::cell::RefCell::new(0.0f64));
    // Liveness guard for raw-mode drain tasks: the queued one-delta
    // timers outlive the driver's early stop; the flag makes them
    // no-ops once the test has restored the signals.
    let alive = std::rc::Rc::new(std::cell::RefCell::new(true));

    // Seed the streaming card so each flush does the real work:
    // hot-tail markdown re-parse + Leptos notify + engine step.
    let seed = flood_seed();
    state.live_text.set(seed.clone());
    state.live_reasoning.set(String::new());
    state.streaming.set(true);
    crate::pile::on_change();

    // Driver: one 16 ms timer chain that (a) pushes this tick's flood
    // batch — coalesced buffers in the default mode, the old per-delta
    // path in raw mode — (b) samples the tick gap, (c) schedules the
    // 0 ms timer round-trip probe, (d) reschedules itself, and
    // (e) on the last tick restores the signals and reports.
    let flush = make_delta_flush(&state);
    let live_text = state.live_text;
    let live_reasoning = state.live_reasoning;
    let streaming = state.streaming;
    let ds = std::rc::Rc::new(std::cell::RefCell::new(FloodState {
        frame: 0,
        last_ts: None,
        max_gap_ms: 0.0,
        timer_max_ms: 0.0,
        pending: Vec::new(),
        tick: None,
        first: None,
    }));
    let next_frame: std::rc::Rc<std::cell::RefCell<Option<Closure<dyn Fn()>>>> = std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let ns = next_frame.clone();
        let ds = ds.clone();
        let lat = timer_latency.clone();
        let driver = Closure::wrap(Box::new(move || {
            let done = {
                let mut st = ds.borrow_mut();
                st.frame += 1;
                let now = js_sys::Date::now();
                if let Some(last) = st.last_ts {
                    st.max_gap_ms = st.max_gap_ms.max(now - last);
                }
                st.last_ts = Some(now);
                if raw_mode {
                    // Pre-v0.5.23 behaviour: every delta was its own
                    // task (a separate WS message is its own
                    // macrotask), so Leptos flushed BETWEEN deltas —
                    // K full re-parse/DOM cycles per frame. Model that
                    // with K separate timer tasks: a synchronous loop
                    // would coalesce into one cycle and the test would
                    // not discriminate.
                    for _ in 0..FLOOD_CHUNKS_PER_FRAME {
                        let lt = live_text;
                        let st = streaming;
                        let al = alive.clone();
                        crate::pile::leak_timeout(gloo_timers::callback::Timeout::new(
                            0,
                            move || {
                                if !*al.borrow() {
                                    return;
                                }
                                lt.update(|s| s.push_str(FLOOD_CHUNK));
                                st.set(true);
                                crate::pile::on_change();
                            },
                        ));
                    }
                } else {
                    PENDING_TEXT.with(|b| {
                        let mut b = b.borrow_mut();
                        for _ in 0..FLOOD_CHUNKS_PER_FRAME {
                            b.push_str(FLOOD_CHUNK);
                        }
                    });
                    // Drive the production coalesced flush directly:
                    // schedule_delta_flush is rAF-based and rAF never
                    // fires in the headless shell, so the tick invokes
                    // the flush's JS function value instead (same
                    // coalescing contract: one flush per tick, K deltas
                    // inside). The ScopedClosure stays owned by the
                    // driver, so its JS callback stays registered.
                    let f: &js_sys::Function =
                        flush.as_js_value().unchecked_ref::<js_sys::Function>();
                    let _ = f.call0(&wasm_bindgen::JsValue::UNDEFINED);
                }
                // Timer round-trip sample for this tick: scheduled
                // AFTER the flood block, so it is queued behind the
                // raw delta tasks and measures the whole burst.
                let t0v = now;
                let latc = lat.clone();
                let to = gloo_timers::callback::Timeout::new(
                    0,
                    move || {
                        *latc.borrow_mut() = js_sys::Date::now() - t0v;
                    },
                );
                st.pending.push(to);
                st.timer_max_ms = st.timer_max_ms.max(*lat.borrow());
                let done = st.frame >= FLOOD_FRAMES
                    || (raw_mode
                        && st.frame >= FLOOD_EARLY_STOP_FRAMES
                        && (st.max_gap_ms > FLOOD_MAX_GAP_MS
                            || st.timer_max_ms > FLOOD_MAX_TIMER_MS));
                if !done {
                    // Re-trigger the driver through its JS function
                    // value: take the stored closure, clone its JS
                    // reference, hand the owner back to ns (dropping a
                    // ScopedClosure deregisters its JS callback), and
                    // let the next tick's timer call the JS function.
                    let c = ns.borrow_mut().take();
                    if let Some(c) = c {
                        let jsfn = c.as_js_value().clone();
                        ns.borrow_mut().replace(c);
                        st.tick = Some(gloo_timers::callback::Timeout::new(
                            FLOOD_FRAME_MS,
                            move || {
                                let f: &js_sys::Function =
                                    jsfn.unchecked_ref::<js_sys::Function>();
                                let _ = f.call0(&wasm_bindgen::JsValue::UNDEFINED);
                            },
                        ));
                    }
                }
                done
            };
            if done {
                ns.borrow_mut().take(); // deregister (no more frames)
                // Stop raw drain tasks that are still queued: they
                // must not append to the restored text or revive the
                // streaming card.
                *alive.borrow_mut() = false;
                // clone (not move): the driver closure must stay Fn.
                live_text.set(old_text.clone());
                live_reasoning.set(old_reasoning.clone());
                streaming.set(old_streaming);
                drop_pending_deltas();
                crate::pile::on_change();
                let st = ds.borrow();
                let verdict = st.max_gap_ms < FLOOD_MAX_GAP_MS
                    && st.timer_max_ms < FLOOD_MAX_TIMER_MS;
                let reason = if raw_mode {
                    let r = "raw control (pre-fix behaviour — expected to fail)";
                    if st.frame < FLOOD_FRAMES {
                        format!("{r} — stopped early at frame {}", st.frame)
                    } else {
                        r.to_string()
                    }
                } else {
                    "coalesced per-frame flush".to_string()
                };
                let gap = st.max_gap_ms;
                let clk = st.timer_max_ms;
                let frames = st.frame;
                drop(st);
                flood_report(
                    Some(verdict),
                    &reason,
                    gap,
                    clk,
                    frames,
                    live_text.get().len(),
                );
                return;
            }
        }) as Box<dyn Fn()>);
        next_frame.borrow_mut().replace(driver);
    }
    {
        let c = next_frame.borrow_mut().take().expect("driver stored");
        // Timer kick (rAF does not fire in the headless shell — see
        // FLOOD_FRAME_MS). The timer must stay alive until it fires,
        // so it lives in FloodState until the first tick.
        let jsfn = c.as_js_value().clone();
        next_frame.borrow_mut().replace(c);
        ds.borrow_mut().first = Some(gloo_timers::callback::Timeout::new(
            FLOOD_FRAME_MS,
            move || {
                let f: &js_sys::Function =
                    jsfn.unchecked_ref::<js_sys::Function>();
                let _ = f.call0(&wasm_bindgen::JsValue::UNDEFINED);
            },
        ));
    }
}
