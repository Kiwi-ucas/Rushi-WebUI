//! Live event stream: one WebSocket per session (port of JS connectWS/sendWS).
//! Built on web-sys `WebSocket` because gloo-net 0.6 has no `ws` feature.

use std::cell::RefCell;

use leptos::prelude::*;
use serde_json::Value;
use web_sys::{CloseEvent, MessageEvent, WebSocket};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::markdown::normalize_event;
use crate::model::AppState;

thread_local! {
    /// The live socket for the active session; replaced (and closed) on
    /// session switch. Dropping the stored Closures deregisters handlers.
    static WS_LIVE: RefCell<Option<LiveSocket>> = const { RefCell::new(None) };
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
pub fn connect(state: &AppState, session: &str) {
    close_current();
    // A fresh connection owns a fresh live-stream state: no stale
    // streamed text, no phantom running tool cards.
    state.clear_live();

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

    let on_msg = {
        let events = events;
        let ctx_used = ctx_used;
        let rounds_ctxk = rounds_ctxk;
        let loop_running = loop_running;
        let live_text = live_text;
        let live_reasoning = live_reasoning;
        let streaming = streaming;
        let tool_pending = tool_pending;
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
                        // Replay the legacy ctx bookkeeping: ctx_used is
                        // the last assistant usage; each user_message
                        // (except the very first event) closes the
                        // previous round, recording ctx_used as its ctxK.
                        let mut ctx = 0u64;
                        let mut ctxk: Vec<u64> = Vec::new();
                        for (i, ev) in normed.iter().enumerate() {
                            let t = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if t == "assistant_message" {
                                // legacy updateCtxBar: only a truthy input_tokens
                                // count moves the bar.
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
                        ctx_used.set(ctx);
                        rounds_ctxk.set(ctxk);
                        // Rebuild the running tool-call set from history:
                        // tool_call ids with no matching tool_result
                        // (only survives when a loop died mid-tool).
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
                        tool_pending.set(
                            pending
                                .into_iter()
                                .filter(|id| !done.iter().any(|d| d == id))
                                .collect(),
                        );
                        events.set(normed);
                        // Drive the pile engine directly (the Transcript
                        // effect is a second, redundant trigger): park
                        // the view at the last message of the history.
                        crate::pile::on_history_loaded();
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
                            "assistant_message" | "error" => {
                                live_text.set(String::new());
                                live_reasoning.set(String::new());
                                streaming.set(false);
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
                        crate::pile::on_change();
                    }
                    "loop_status" => {
                        let running = item.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
                        let exit = item.get("exit").and_then(|v| v.as_i64());
                        let stopped = item.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false);
                        loop_running.set(running);
                        // An unexpected death (non-zero exit or killed by a
                        // signal, not our stop command) gets a card so the
                        // user can see the loop died and where to look.
                        let unexpected = !running
                            && !stopped
                            && match exit {
                                Some(c) => c != 0,
                                None => true,
                            };
                        if unexpected {
                            // The loop died mid-model-call: drop any half-
                            // streamed text so a stale "generating" card
                            // does not sit next to the error card.
                            live_text.set(String::new());
                            live_reasoning.set(String::new());
                            streaming.set(false);
                            let detail = match exit {
                                Some(c) => format!("exit {c}"),
                                None => "killed by signal".to_string(),
                            };
                            let ev = serde_json::json!({
                                "type": "error",
                                "ts": crate::timeutil::now_iso(),
                                "message": format!(
                                    "loop stopped unexpectedly ({detail}); check loop.stderr in the session folder"
                                ),
                            });
                            events.update(|old| old.push(ev));
                            crate::pile::on_change();
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
                            match dkind {
                                "text" => {
                                    live_text.update(|s| s.push_str(delta));
                                    streaming.set(true);
                                }
                                "reasoning" => {
                                    live_reasoning.update(|s| s.push_str(delta));
                                    streaming.set(true);
                                }
                                _ => {}
                            }
                            // Let the pile follow the growing card
                            // (rAF-throttled; a no-op if already parked).
                            crate::pile::on_change();
                        }
                    }
                    _ => {}
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>)
    };

    let on_close = {
        let ws_status = ws_status;
        Closure::wrap(
            Box::new(move |_e: CloseEvent| {
                ws_status.set("disconnected".to_string());
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
