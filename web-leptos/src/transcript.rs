//! Transcript + event cards + markdown/highlight views
//! (port of renderEvent, renderMD, highlightCode, and the transcript DOM).

use leptos::prelude::*;
use serde_json::Value;

use crate::markdown as md;
use crate::model::AppState;

// ── transcript (the #transcript container) ───────────────────────
/// Card index list for the `For` (avoids a turbofish inside the
/// view! macro, which its parser chokes on).
fn all_card_indices(n: usize) -> Vec<usize> {
    (0..n).collect()
}

#[component]
pub fn Transcript(state: AppState) -> impl IntoView {
    let events = state.events;

    // Phase 4: the scroll/pile engine drives the DOM of #transcript
    // (fold/compact rows, cut-line shrink, auto-scroll) off every
    // change of the event stream / view round / active session.
    // The effect's first tick lands after mount_to, so the DOM is
    // ready and the engine can register its listeners lazily.
    Effect::new(move || {
        let _ = events.get();
        let _ = state.view_round.get();
        let _ = state.active_session.get();
        crate::pile::init(state);
        crate::pile::on_change();
    });

    view! {
        <div id="transcript">
            <For
                // All cards stay in the DOM (even in a round view); the
                // pile engine adds .hid to cards after the viewed
                // summary — the legacy structure.
                each=move || all_card_indices(events.get().len())
                key=|&i| i
                children=move |i| {
                    event_card_view(i, events, state)
                }
            />
            // Streaming card: the in-flight model call, rendered live
            // off the `.model-stream` side channel. No `event` class
            // token, so the pile engine's iter_cards ignores it; it
            // rides in the flow at the bottom and is replaced by the
            // final assistant_message card when that event lands.
            <Show
                when=move || state.streaming.get()
                fallback=|| ()
            >
                { live_card_view(state) }
            </Show>
            <div id="scroll-spacer" />
        </div>
    }
}

/// The in-progress assistant card: streamed thinking block (open) +
/// streamed body text. Both children are reactive text nodes off the
/// `live_reasoning` / `live_text` signals, so each delta updates the
/// card in place. Plain text while streaming; the final card renders
/// markdown once the `assistant_message` event lands.
fn live_card_view(state: AppState) -> AnyView {
    let live_text = state.live_text;
    let live_reasoning = state.live_reasoning;
    let v = view! {
        <div class="ev-live enter">
            <span class="ev-header">
                <span class="ev-type">agent</span>
                <span class="ev-running-dot" />
            </span>
            <Show
                when=move || !live_reasoning.get().is_empty()
                fallback=|| ()
            >
                <details class="ev-thinking" open>
                    <summary>{ "thinking…" }</summary>
                    <div class="ev-thinking-body">
                        { move || live_reasoning.get() }
                    </div>
                </details>
            </Show>
            <div class="ev-content ev-live-text">
                { move || live_text.get() }
            </div>
        </div>
    };
    v.into_any()
}

// ── single event card ─────────────────────────────────────────────
fn event_card_view(key: usize, events: RwSignal<Vec<Value>>, state: AppState) -> AnyView {
    let ev = events.with(|v| v.get(key).cloned()).unwrap_or(Value::Null);
    let t = ev.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if t.is_empty() || t == "ext_status" {
        // Legacy addEvent returns early for ext_status: no card, so the
        // pile engine sees no DOM node for it (card k != event k).
        return ().into_any();
    }

    let is_err = t == "tool_result"
        && ev.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);

    let cls = match t.as_str() {
        "user_message" => "event ev-user",
        "assistant_message" => "event ev-assistant",
        "tool_call" => "event ev-tool-call",
        "tool_result" if is_err => "event ev-tool-result error",
        "tool_result" => "event ev-tool-result",
        "error" => "event ev-error",
        "compaction_started" | "compaction_summary" | "compaction_failed" => "event ev-compaction",
        "context_exhausted" => "event ev-ctx-exhausted",
        "approval_request" | "approval" => "event ev-approval",
        "rewind" => "event ev-rewind",
        "user_message_retract" => "event ev-retract",
        "ext_status" => "event ev-ext-status",
        _ => "event",
    };
    // `.enter` drives the mount fade-in (style.css `.event.enter`). The
    // class is removed on `animationend` by the pile engine's delegated
    // listener; if that never fires (reduced motion) the class is
    // harmless — the animation is disabled there too.
    let cls = format!("{cls} enter");

    let brief = match t.as_str() {
        "user_message" => format!(
            "you \u{b7} {}",
            md::brief_text(ev.get("content").and_then(|v| v.as_str()).unwrap_or(""), 20)
        ),
        "assistant_message" => format!(
            "agent \u{b7} {}",
            md::brief_text(ev.get("content").and_then(|v| v.as_str()).unwrap_or(""), 20)
        ),
        "tool_call" => format!(
            "tool \u{b7} {}",
            ev.get("name").and_then(|v| v.as_str()).unwrap_or("tool")
        ),
        "tool_result" => {
            if is_err { "tool \u{b7} error".into() } else { "tool \u{b7} done".into() }
        }
        _ => t.replace('_', " "),
    };

    let ts_str = crate::timeutil::ts(ev.get("ts").and_then(|v| v.as_str()).unwrap_or(""));

    let body = ev_body(&ev, &t, state, events);
    let type_word = t.replace('_', " ");

    let card = view! {
        <div class=cls data-brief=brief>
            <span class="ev-header">
                <span class="ev-type">{ type_word }</span>
                <span class="ev-time">{ ts_str }</span>
            </span>
            { body }
        </div>
    };
    card.into_any()
}

/// Build the inner content for each event type.
fn ev_body(ev: &Value, t: &str, state: AppState, events: RwSignal<Vec<Value>>) -> AnyView {
    match t {
        "ext_status" => view! { <span /> }.into_any(),

        "user_message" => {
            let content = ev.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let queue = ev.get("queue").and_then(|v| v.as_str()).map(|s| s.to_string());
            let badge: AnyView = match queue {
                Some(q) => view! { <div class="ev-badge">{ format!("queued: {q}") }</div> }.into_any(),
                None => view! { <div /> }.into_any(),
            };
            let v = view! {
                { badge }
                <div class="ev-content">{ md_blocks_view(content) }</div>
            };
            v.into_any()
        }

        "assistant_message" => {
            let content = ev.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let reasoning_text = ev
                .get("reasoning")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| md::reasoning_text(item))
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default();
            let has_thinking = !reasoning_text.is_empty();

            let tool_calls: Vec<(String, Vec<String>)> = ev
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|tc| {
                            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let args = tc
                                .get("arguments")
                                .and_then(|v| v.as_object())
                                .map(|o| o.keys().cloned().collect())
                                .unwrap_or_default();
                            (name, args)
                        })
                        .collect()
                })
                .unwrap_or_default();

            let usage = ev.get("usage").cloned();
            let usage_line = usage.as_ref().and_then(|u| {
                let mut parts: Vec<String> = Vec::new();
                if let Some(n) = u.get("input_tokens").and_then(|v| v.as_u64()) {
                    parts.push(format!("{n} in"));
                }
                if let Some(n) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                    parts.push(format!("{n} out"));
                }
                if let Some(n) = u.get("cached_tokens").and_then(|v| v.as_u64()) {
                    parts.push(format!("{n} cached"));
                }
                let s = parts.join(" | ");
                if s.is_empty() { None } else { Some(s) }
            });

            if let Some(u) = &usage {
                if let Some(n) = u.get("input_tokens").and_then(|v| v.as_u64()) {
                    state.ctx_used.set(n);
                }
            }

            let tc_views: Vec<AnyView> = tool_calls
                .iter()
                .map(|(name, args)| {
                    let label = format!("{name} ({})", args.join(", "));
                    view! { <div class="ev-tc-chip">{ label }</div> }.into_any()
                })
                .collect();

            let thinking: AnyView = if has_thinking {
                let t = view! {
                    <details class="ev-thinking" open>
                        <summary>{ "thinking" }</summary>
                        <div class="ev-thinking-body">{ reasoning_text }</div>
                    </details>
                };
                t.into_any()
            } else {
                view! { <div /> }.into_any()
            };
            let usage_badge: AnyView = match usage_line {
                Some(u) => view! { <div class="ev-usage">{ u }</div> }.into_any(),
                None => view! { <div /> }.into_any(),
            };

            let v = view! {
                { thinking }
                <div class="ev-content">{ md_blocks_view(content) }</div>
                { tc_views }
                { usage_badge }
            };
            v.into_any()
        }

        "tool_call" => {
            let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
            let args = ev.get("arguments").cloned().unwrap_or(Value::Object(Default::default()));
            let sum = md::summarize_args(&args);
            let tool_line = if sum.is_empty() { name.clone() } else { format!("{name} {sum}") };
            let arg_keys: Vec<String> = args.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
            let arg_count = arg_keys.len();
            let args_json = serde_json::to_string_pretty(&args).unwrap_or_default();

            let args_detail: AnyView = if !arg_keys.is_empty() {
                let d = view! {
                    <details class="ev-tool-args">
                        <summary>{ format!("args ({arg_count})") }</summary>
                        <pre class="ev-args">{ args_json }</pre>
                    </details>
                };
                d.into_any()
            } else {
                view! { <div /> }.into_any()
            };
            // Running marker: shown while the matching tool_result has
            // not landed yet (ws.rs maintains tool_pending).
            let cid = ev.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let pending = state.tool_pending;
            let running = Memo::new(move |_prev: Option<&bool>| {
                pending.with(|v| v.iter().any(|p| p == cid.as_str()))
            });
            let running_view: AnyView = view! {
                <Show when=move || running.get() fallback=|| ()>
                    <span class="ev-running">{ "running…" }</span>
                </Show>
            }
            .into_any();
            let v = view! {
                <div class="ev-tool-line">{ tool_line } { running_view }</div>
                { args_detail }
            };
            v.into_any()
        }

        "tool_result" => {
            let text = result_text(ev);
            let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let summary = if flat.is_empty() {
                if is_err_flag(ev) { "result (error)".to_string() } else { "result (empty)".to_string() }
            } else if flat.chars().count() > 60 {
                let mut t: String = flat.chars().take(60).collect();
                t.push('\u{2026}');
                format!("result \u{b7} {t}")
            } else {
                format!("result \u{b7} {flat}")
            };
            let tool_log = ev.get("tool_log").and_then(|v| v.as_str()).map(|s| s.to_string());
            let tool_log_display = tool_log.clone();
            let result_text_display = if text.is_empty() { "(empty)".to_string() } else { text.clone() };

            view! {
                <details class="ev-result-det">
                    <summary>{ summary }</summary>
                    <pre class="ev-result">{ result_text_display }</pre>
                </details>
                <Show when=move || tool_log.is_some() fallback=|| ()>
                    <div class="ev-tool-log">{ format!("tool log: {}", tool_log_display.as_deref().unwrap_or("")) }</div>
                </Show>
            }.into_any()
        }

        "error" => {
            let msg = ev.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
            view! { <div class="ev-err-msg">{ msg }</div> }.into_any()
        }

        "compaction_started" | "compaction_summary" | "compaction_failed" => {
            let msg = match t {
                "compaction_started" => format!(
                    "Compaction in progress\u{2026} ({})",
                    ev.get("reason").and_then(|v| v.as_str()).unwrap_or("")
                ),
                "compaction_summary" => format!(
                    "Compacted. {}",
                    ev.get("summary")
                        .and_then(|v| v.as_str())
                        .or_else(|| ev.get("message").and_then(|v| v.as_str()))
                        .unwrap_or("Summary generated.")
                ),
                _ => format!(
                    "Compaction failed: {}",
                    ev.get("message")
                        .and_then(|v| v.as_str())
                        .or_else(|| ev.get("error").and_then(|v| v.as_str()))
                        .unwrap_or("")
                ),
            };
            view! { <div class="ev-comp-text">{ msg }</div> }.into_any()
        }

        "context_exhausted" => {
            let msg = format!(
                "Context exhausted. {}",
                ev.get("message").and_then(|v| v.as_str()).unwrap_or("Compaction or truncation required.")
            );
            view! { <div class="ev-ctx-msg">{ msg }</div> }.into_any()
        }

        "approval_request" => {
            let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let msg = ev
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| ev.get("content").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Approval requested: {id}"));

            // id lives in a signal (Copy) so the Show children closure
            // stays `Fn` across re-renders; handlers capture the Copy
            // signal, never the String.
            let id_sig = RwSignal::new(id);

            let resolved = Memo::new(move |_prev: Option<&bool>| {
                events.with(|v| {
                    v.iter().any(|e| {
                        e.get("type").and_then(|t| t.as_str()) == Some("approval")
                            && e.get("id").and_then(|i| i.as_str()) == Some(id_sig.get().as_str())
                    })
                })
            });

            view! {
                <div class="ev-approval-msg">{ msg }</div>
                <Show when=move || !resolved.get() fallback=|| ()>
                    <div class="ev-approval-actions">
                        <button
                            class="btn-approve"
                            on:click=move |_| {
                                if let Some(s) = state.active_session.get() {
                                    crate::ws::send_command(
                                        &s,
                                        &serde_json::json!({ "kind": "approval", "id": id_sig.get(), "decision": "approve" }),
                                    );
                                }
                            }
                        >
                            { "Approve" }
                        </button>
                        <button
                            class="btn-deny"
                            on:click=move |_| {
                                if let Some(s) = state.active_session.get() {
                                    crate::ws::send_command(
                                        &s,
                                        &serde_json::json!({ "kind": "approval", "id": id_sig.get(), "decision": "deny" }),
                                    );
                                }
                            }
                        >
                            { "Deny" }
                        </button>
                    </div>
                </Show>
            }.into_any()
        }

        "approval" => {
            let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let decision = ev.get("decision").and_then(|v| v.as_str()).unwrap_or("resolved").to_string();
            let allow = decision == "approve" || decision == "allow";
            let badge_cls = if allow { "ev-badge-resolved allow" } else { "ev-badge-resolved deny" };
            let msg_text = format!("Approval ({id}): {decision}");
            let decision_display = decision.clone();
            view! {
                <div class="ev-approval-resolved-msg">{ msg_text }</div>
                <div class=badge_cls>{ decision_display }</div>
            }.into_any()
        }

        "rewind" => {
            let target = ev.get("target_seq").and_then(|v| v.as_i64()).map(|n| n.to_string()).unwrap_or_else(|| "?".into());
            let mode = ev.get("mode").and_then(|v| v.as_str()).unwrap_or("before");
            view! { <div class="ev-rewind-msg">{ format!("\u{21a9} Rewound to step {target} ({mode})") }</div> }.into_any()
        }

        "user_message_retract" => {
            let target = ev.get("target").and_then(|v| v.as_i64()).map(|n| n.to_string()).unwrap_or_else(|| "?".into());
            view! { <div class="ev-retract-msg">{ format!("Retracted message #{target}") }</div> }.into_any()
        }

        _ => {
            let raw = serde_json::to_string_pretty(ev).unwrap_or_default();
            view! {
                <div style="border-left:3px solid var(--text-muted);">
                    <div class="ev-args">{ raw }</div>
                </div>
            }.into_any()
        }
    }
}

fn is_err_flag(ev: &Value) -> bool {
    ev.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)
}

fn result_text(ev: &Value) -> String {
    let v = ev.get("value");
    match v {
        Some(val) if val.is_object() => {
            if let Some(t) = val.get("text").and_then(|x| x.as_str()) {
                t.to_string()
            } else if let Some(d) = val.get("details") {
                if d.is_string() {
                    d.as_str().unwrap_or("").to_string()
                } else {
                    serde_json::to_string_pretty(d).unwrap_or_default()
                }
            } else if let Some(e) = val.get("error") {
                e.as_str().map(|s| s.to_string()).unwrap_or_default()
            } else {
                serde_json::to_string_pretty(val).unwrap_or_default()
            }
        }
        _ => ev.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    }
}

// ── markdown blocks → Leptos views ───────────────────────────────
fn md_blocks_view(src: String) -> impl IntoView {
    let blocks = md::parse_md(&src);
    let indices: Vec<usize> = (0..blocks.len()).collect();
    view! {
        <div class="md">
            <For
                each=move || indices.clone()
                key=|&i| i
                children=move |i| {
                    md_block_view(&blocks[i])
                }
            />
        </div>
    }
}

fn md_block_view(b: &md::MdBlock) -> AnyView {
    match b {
        md::MdBlock::H1(s) => view! { <h1>{ s.clone() }</h1> }.into_any(),
        md::MdBlock::H2(s) => view! { <h2>{ s.clone() }</h2> }.into_any(),
        md::MdBlock::H3(s) => view! { <h3>{ s.clone() }</h3> }.into_any(),
        md::MdBlock::Hr => view! { <hr /> }.into_any(),
        md::MdBlock::Para(inls) => {
            let inls = inls.clone();
            view! { <p>{ inlines_view(inls) }</p> }.into_any()
        }
        md::MdBlock::Ul(items) => {
            let items = items.clone();
            let indices: Vec<usize> = (0..items.len()).collect();
            view! {
                <ul>
                    <For
                        each=move || indices.clone()
                        key=|&i| i
                        children=move |i| {
                            view! { <li>{ inlines_view(items[i].clone()) }</li> }
                        }
                    />
                </ul>
            }.into_any()
        }
        md::MdBlock::Ol(items) => {
            let items = items.clone();
            let indices: Vec<usize> = (0..items.len()).collect();
            view! {
                <ol>
                    <For
                        each=move || indices.clone()
                        key=|&i| i
                        children=move |i| {
                            view! { <li>{ inlines_view(items[i].clone()) }</li> }
                        }
                    />
                </ol>
            }.into_any()
        }
        md::MdBlock::Code(lang, code) => {
            let lang = lang.clone();
            let code = code.clone();
            let segs = md::highlight(&code);
            view! {
                <pre><code class=format!("lang-{lang}")>{ hl_view(segs) }</code></pre>
            }.into_any()
        }
    }
}

fn inlines_view(inls: Vec<md::MdInline>) -> impl IntoView {
    let indices: Vec<usize> = (0..inls.len()).collect();
    view! {
        <For
            each=move || indices.clone()
            key=|&i| i
            children=move |i| {
                match &inls[i] {
                    md::MdInline::Text(t) => view! { { t.clone() } }.into_any(),
                    md::MdInline::Bold(t) => view! { <strong>{ t.clone() }</strong> }.into_any(),
                    md::MdInline::Em(t) => view! { <em>{ t.clone() }</em> }.into_any(),
                    md::MdInline::Code(t) => view! { <code>{ t.clone() }</code> }.into_any(),
                }
            }
        />
    }
}

fn hl_view(segs: Vec<md::HlSeg>) -> impl IntoView {
    let indices: Vec<usize> = (0..segs.len()).collect();
    view! {
        <For
            each=move || indices.clone()
            key=|&i| i
            children=move |i| {
                match &segs[i].class {
                    Some(cls) => view! { <span class=cls.clone()>{ segs[i].text.clone() }</span> }.into_any(),
                    None => view! { { segs[i].text.clone() } }.into_any(),
                }
            }
        />
    }
}
