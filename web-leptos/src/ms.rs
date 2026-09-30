//! Model settings panel (v0.5.42; master-detail again since v0.5.47).
//!
//! Organised by **provider**, not by model: the left rail lists one card
//! per `(base_url, api_key_env)` pair, the right pane edits the selected
//! one and the models it serves. The kernel's config is flat — every
//! `[model."<name>"]` entry carries its own `base_url` / `api_key_env` —
//! so the grouping is a view: a pane's fields are written to every entry
//! under it, and the save flattens back to entries.
//!
//! Two things drive the form's shape:
//!
//! - Everything numeric is edited as text (`""` = inherit / drop the
//!   key), because the kernel silently falls back to a default on a type
//!   mismatch.
//! - The reasoning levels on offer are narrowed to the ones the model
//!   family accepts (see [`effort_levels_for`]). The reference harness
//!   (dsh / pi-ai) does the same from a generated per-model catalog with
//!   an explicit `thinkingLevelMap`; we have no catalog, so we infer the
//!   family from the model id and give the full list behind "advanced".
//!
//! Which model a session actually runs is NOT decided here: the new
//! session dialog picks it, and the session card's chip changes it. The
//! card's green dot is the provider's reachability, nothing else.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;
use web_sys::MouseEvent;

use crate::api;
use crate::model::{AppState, Globals, ModelEntry, ModelSettingsView};
use crate::ui::after_dispatch;

/// Every level the kernel knows (`crates/rushi/src/model_settings.rs`
/// passes them through verbatim).
const ALL_EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Reasoning levels a model family accepts.
///
/// dsh/pi-ai reads this from a generated catalog entry's
/// `thinkingLevelMap` (`{"minimal": null, "low": "low", ...}` — a level
/// mapped to null is one the provider does not take, so its UI does not
/// offer it). We have no catalog and the providers expose no such
/// metadata (`GET /v1/models` on this machine's sglang returns only the
/// id and `max_model_len`), so the family is inferred from the model id.
/// `None` = "cannot tell": the form shows every level.
pub(crate) fn effort_levels_for(model_id: &str) -> Option<&'static [&'static str]> {
    let id = model_id.to_ascii_lowercase();
    let has = |p: &str| id.contains(p);
    if has("deepseek") || has("glm") || has("kimi") || has("moonshot") || has("minimax") {
        // dsh's deepseek map: minimal/medium are null there.
        Some(&["low", "high", "max"])
    } else if has("qwen") {
        Some(&["low", "medium", "high", "xhigh", "max"])
    } else if has("gpt-5") || has("o1") || has("o3") || has("o4") {
        Some(&["minimal", "low", "medium", "high"])
    } else if has("grok") {
        Some(&["low", "medium", "high"])
    } else {
        None
    }
}

/// The levels to offer for a model: the inferred set, or every level when
/// the family cannot be told (never hide the control entirely — that
/// would leave no way to set an effort for an unknown model).
pub(crate) fn effort_choices(model_id: &str) -> Vec<&'static str> {
    effort_levels_for(model_id).map(|l| l.to_vec()).unwrap_or_else(|| ALL_EFFORTS.to_vec())
}

/// One provider model from `GET {base_url}/v1/models`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
struct FetchedModel {
    id: String,
    #[serde(default)]
    context_window: Option<u64>,
}

/// One model inside a provider card. Text drafts: `""` = key absent.
#[derive(Clone, Debug, Default, PartialEq)]
struct ModelDraft {
    name: String,
    model_id: String,
    context_tokens: String,
    max_output_tokens: String,
    reasoning_effort: String,
    timeout_s: String,
    estimate_chars_per_token: String,
    vision: bool,
    extra_keys: Vec<String>,
    /// UI only: is the detail block open?
    expanded: bool,
    /// UI only: has the user opened the full level list for this model?
    advanced: bool,
}

/// One provider card. `base_url` / `api_key_env` are shared by every
/// model inside (that is what makes them one provider).
#[derive(Clone, Debug, Default, PartialEq)]
struct ProviderDraft {
    base_url: String,
    api_key_env: String,
    /// UI only: the write-only key box.
    key_input: String,
    models: Vec<ModelDraft>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Draft {
    /// Round-tripped untouched: the panel no longer exposes the active
    /// entry (the new-session dialog and the session card chip pick it).
    active: String,
    globals: Globals,
    providers: Vec<ProviderDraft>,
}

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn n(v: &Option<u64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn parse_n(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        t.parse::<u64>().ok()
    }
}

fn opt(v: &str) -> Option<String> {
    let t = v.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

impl ModelDraft {
    fn from_entry(e: &ModelEntry) -> Self {
        Self {
            name: e.name.clone(),
            model_id: s(&e.model_id),
            context_tokens: n(&e.context_tokens),
            max_output_tokens: n(&e.max_output_tokens),
            reasoning_effort: s(&e.reasoning_effort),
            timeout_s: n(&e.timeout_s),
            estimate_chars_per_token: n(&e.estimate_chars_per_token),
            vision: e.vision == Some(true),
            extra_keys: e.extra_keys.clone(),
            expanded: false,
            advanced: false,
        }
    }

    fn to_entry(&self, provider: &ProviderDraft) -> ModelEntry {
        ModelEntry {
            name: self.name.trim().to_string(),
            model_id: opt(&self.model_id),
            base_url: opt(&provider.base_url),
            api_key_env: opt(&provider.api_key_env),
            context_tokens: parse_n(&self.context_tokens),
            max_output_tokens: parse_n(&self.max_output_tokens),
            reasoning_effort: opt(&self.reasoning_effort),
            vision: if self.vision { Some(true) } else { None },
            timeout_s: parse_n(&self.timeout_s),
            estimate_chars_per_token: parse_n(&self.estimate_chars_per_token),
            extra_keys: self.extra_keys.clone(),
        }
    }
}

impl Draft {
    fn from_view(v: &ModelSettingsView) -> Self {
        // Group the flat entries by the pair that defines a provider,
        // keeping the config's own order.
        let mut providers: Vec<ProviderDraft> = Vec::new();
        for e in &v.entries {
            let base_url = s(&e.base_url);
            let api_key_env = s(&e.api_key_env);
            let card = match providers
                .iter_mut()
                .find(|p| p.base_url == base_url && p.api_key_env == api_key_env)
            {
                Some(p) => p,
                None => {
                    providers.push(ProviderDraft {
                        base_url,
                        api_key_env,
                        key_input: String::new(),
                        models: Vec::new(),
                    });
                    providers.last_mut().unwrap()
                }
            };
            card.models.push(ModelDraft::from_entry(e));
        }
        // One model open by default so the panel never looks empty.
        if let Some(p) = providers.first_mut() {
            if let Some(m) = p.models.first_mut() {
                m.expanded = true;
            }
        }
        Self {
            active: v.active.clone(),
            globals: v.globals.clone(),
            providers,
        }
    }

    fn to_payload(&self, view: &ModelSettingsView) -> ModelSettingsView {
        let entries = self
            .providers
            .iter()
            .flat_map(|p| p.models.iter().map(|m| m.to_entry(p)))
            .collect();
        ModelSettingsView {
            active: self.active.clone(),
            entries,
            globals: self.globals.clone(),
            config_path: view.config_path.clone(),
            mirror_path: view.mirror_path.clone(),
            key_env_present: view.key_env_present.clone(),
            key_env_stored: view.key_env_stored.clone(),
            effective: view.effective.clone(),
            notes: view.notes.clone(),
        }
    }
}

/// The probe cache key: a result only counts for the exact provider
/// values it was measured against, so editing base_url or api_key_env
/// clears the dot back to unknown.
fn card_key(p: &ProviderDraft) -> String {
    format!("{}\u{1}{}", p.base_url.trim(), p.api_key_env.trim())
}

/// One labelled text input bound to a draft field.
#[component]
fn TextRow(
    label: &'static str,
    id: &'static str,
    placeholder: &'static str,
    value: Signal<String>,
    on_input: Callback<String>,
) -> impl IntoView {
    view! {
        <label class="ns-label" for=id>{ label }</label>
        <input
            id=id
            class="ns-input"
            placeholder=placeholder
            prop:value=value
            on:input=move |ev| on_input.run(event_target_value(&ev))
        />
    }
}

#[component]
pub fn ModelSettingsDialog(state: AppState) -> impl IntoView {
    let open = state.model_open;
    let view = state.model_view;
    let err = state.model_err;
    let saved = state.model_saved;
    let busy = state.model_busy;
    let draft = RwSignal::new(Draft::default());
    // Probe results, keyed by `card_key`.
    let probes = RwSignal::new(std::collections::HashMap::<String, Value>::new());
    let fetched = RwSignal::new(std::collections::HashMap::<String, Vec<FetchedModel>>::new());
    let fetch_err = RwSignal::new(std::collections::HashMap::<String, String>::new());
    let fetching = RwSignal::new(std::collections::HashMap::<String, bool>::new());
    let key_busy = RwSignal::new(false);
    let key_msg = RwSignal::new(None::<String>);
    // Which provider card the right-hand pane is editing.
    let sel = RwSignal::new(0usize);

    // Probe provider cards: all of them when the panel opens, one when
    // its ↻ is pressed.
    let probe_cards = move |only: Option<usize>| {
        let providers = draft.get_untracked().providers;
        for (ci, p) in providers.into_iter().enumerate() {
            if only.is_some_and(|o| o != ci) {
                continue;
            }
            let key = card_key(&p);
            if p.base_url.trim().is_empty() {
                continue;
            }
            probes.update(|m| {
                m.insert(key.clone(), serde_json::json!({ "pending": true }));
            });
            let base = p.base_url.trim().to_string();
            let env = p.api_key_env.trim().to_string();
            let env_opt = (!env.is_empty()).then(|| env.clone());
            let first_model = p.models.first().map(|m| m.model_id.clone());
            let mid = first_model
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty());
            spawn_local(async move {
                let r = api::probe_model(&base, env_opt.as_deref(), mid.as_deref())
                    .await
                    .unwrap_or_else(|e| serde_json::json!({ "ok": false, "detail": e }));
                probes.update(|m| {
                    m.insert(key, r);
                });
            });
        }
    };

    let reload = move || {
        err.set(None);
        saved.set(None);
        fetched.set(std::collections::HashMap::new());
        fetch_err.set(std::collections::HashMap::new());
        spawn_local(async move {
            match api::load_model().await {
                Ok(v) => {
                    draft.set(Draft::from_view(&v));
                    sel.set(0);
                    state
                        .model_names
                        .set(v.entries.iter().map(|e| e.name.clone()).collect());
                    view.set(Some(v));
                    probe_cards(None);
                }
                Err(e) => err.set(Some(e)),
            }
        });
    };

    // Pull a fresh snapshot whenever the panel opens.
    Effect::new(move |_| {
        if open.get() {
            reload();
        }
    });

    let close = move || {
        after_dispatch(move || open.set(false));
    };

    let save = move || {
        let Some(v) = view.get() else { return };
        let payload = draft.get().to_payload(&v);
        busy.set(true);
        err.set(None);
        spawn_local(async move {
            match api::save_model(&payload).await {
                Ok(out) => {
                    saved.set(Some(out));
                    if let Ok(fresh) = api::load_model().await {
                        state
                            .model_names
                            .set(fresh.entries.iter().map(|e| e.name.clone()).collect());
                        draft.set(Draft::from_view(&fresh));
                        view.set(Some(fresh));
                    }
                    probe_cards(None);
                }
                Err(e) => err.set(Some(e)),
            }
            busy.set(false);
        });
    };

    // Store the pasted key for one card, under its api_key_env name.
    let save_key = move |ci: usize| {
        let Some(p) = draft.get_untracked().providers.get(ci).cloned() else {
            return;
        };
        let name = if p.api_key_env.trim().is_empty() {
            "MODEL_API_KEY".to_string()
        } else {
            p.api_key_env.trim().to_string()
        };
        let value = p.key_input.trim().to_string();
        if value.is_empty() {
            key_msg.set(Some("paste a key first".to_string()));
            return;
        }
        key_busy.set(true);
        key_msg.set(None);
        spawn_local(async move {
            match api::set_model_key(&name, Some(&value)).await {
                Ok(()) => {
                    draft.update(|d| {
                        if let Some(p) = d.providers.get_mut(ci) {
                            p.key_input.clear();
                        }
                    });
                    key_msg.set(Some(format!("saved for {name}")));
                    if let Ok(v) = api::load_model().await {
                        view.set(Some(v));
                    }
                    probe_cards(None);
                }
                Err(e) => key_msg.set(Some(e)),
            }
            key_busy.set(false);
        });
    };

    let clear_key = move |ci: usize| {
        let Some(p) = draft.get_untracked().providers.get(ci).cloned() else {
            return;
        };
        let name = if p.api_key_env.trim().is_empty() {
            "MODEL_API_KEY".to_string()
        } else {
            p.api_key_env.trim().to_string()
        };
        key_busy.set(true);
        key_msg.set(None);
        spawn_local(async move {
            match api::set_model_key(&name, None).await {
                Ok(()) => {
                    key_msg.set(Some(format!("cleared {name}")));
                    if let Ok(v) = api::load_model().await {
                        view.set(Some(v));
                    }
                    probe_cards(None);
                }
                Err(e) => key_msg.set(Some(e)),
            }
            key_busy.set(false);
        });
    };

    // Ask the provider what it serves, then let the user add rows.
    let fetch_models = move |ci: usize| {
        let Some(p) = draft.get_untracked().providers.get(ci).cloned() else {
            return;
        };
        let key = card_key(&p);
        let base = p.base_url.trim().to_string();
        if base.is_empty() {
            fetch_err.update(|m| {
                m.insert(key, "base_url is empty".to_string());
            });
            return;
        }
        let env_opt = opt(&p.api_key_env);
        fetching.update(|m| {
            m.insert(key.clone(), true);
        });
        fetch_err.update(|m| {
            m.remove(&key);
        });
        spawn_local(async move {
            let payload = serde_json::json!({ "base_url": base, "api_key_env": env_opt });
            let req = gloo_net::http::Request::post("/api/model/models")
                .header("Content-Type", "application/json")
                .body(payload.to_string())
                .map_err(|e| e.to_string());
            let result = match req {
                Ok(r) => match r.send().await {
                    Ok(res) => res.text().await.map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                },
                Err(e) => Err(e),
            };
            match result {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) => {
                        let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
                        let detail = v
                            .get("detail")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        if ok {
                            let models: Vec<FetchedModel> = v
                                .get("models")
                                .cloned()
                                .and_then(|m| serde_json::from_value(m).ok())
                                .unwrap_or_default();
                            if models.is_empty() {
                                fetch_err.update(|m| {
                                    m.insert(key.clone(), "the provider listed no models".into());
                                });
                            }
                            fetched.update(|m| {
                                m.insert(key.clone(), models);
                            });
                        } else {
                            fetch_err.update(|m| {
                                m.insert(key.clone(), detail);
                            });
                        }
                    }
                    Err(e) => {
                        fetch_err.update(|m| {
                            m.insert(key.clone(), format!("bad response: {e}"));
                        });
                    }
                },
                Err(e) => {
                    fetch_err.update(|m| {
                        m.insert(key, e);
                    });
                }
            }
            fetching.update(|m| {
                m.insert(String::new(), false);
            });
        });
    };

    view! {
        <Show when=move || open.get() fallback=|| ()>
            <div id="ms-backdrop" on:click=move |_| close()>
                <div id="ms-dialog" on:click=move |e: MouseEvent| e.stop_propagation()>
                    <div class="ms-head">
                        <span class="ns-title">{ "Model settings" }</span>
                        <span class="ms-path" title=move || {
                            let v = view.get();
                            let cfg = v.as_ref().map(|v| v.config_path.clone()).unwrap_or_default();
                            let mir = v
                                .as_ref()
                                .and_then(|v| v.mirror_path.clone())
                                .map(|m| format!("\nmirror: {m}"))
                                .unwrap_or_default();
                            format!("{cfg}{mir}")
                        }>{ move || view.get().map(|v| v.config_path).unwrap_or_default() }</span>
                    </div>

                    <div class="ms-cols">
                        <div class="ms-list">
                        { move || {
                            draft
                                .get()
                                .providers
                                .iter()
                                .enumerate()
                                .map(|(ci, p)| {
                                    // Everything below reads the card out
                                    // of the draft by index: the closures
                                    // stay `Copy` (they capture only
                                    // signals and scalars), which the
                                    // nested `view!` closures require.
                                    let card_of = move || {
                                        draft
                                            .get()
                                            .providers
                                            .get(ci)
                                            .cloned()
                                            .unwrap_or_default()
                                    };
                                    let k = move || card_key(&card_of());
                                    let reach = move || {
                                        probes
                                            .get()
                                            .get(&k())
                                            .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
                                    };
                                    let pending = move || {
                                        probes
                                            .get()
                                            .get(&k())
                                            .and_then(|v| v.get("pending").and_then(|o| o.as_bool()))
                                            .unwrap_or(false)
                                    };
                                    let detail = move || {
                                        probes
                                            .get()
                                            .get(&k())
                                            .and_then(|v| v.get("detail").and_then(|d| d.as_str()))
                                            .unwrap_or("")
                                            .to_string()
                                    };
                                    // The card's two lines: the name the
                                    // sessions show (its first model's),
                                    // and the endpoint it talks to.
                                    let label = {
                                        let n = p
                                            .models
                                            .iter()
                                            .map(|m| m.name.trim().to_string())
                                            .find(|n| !n.is_empty())
                                            .unwrap_or_default();
                                        if n.is_empty() { "New provider".to_string() } else { n }
                                    };
                                    let sub = {
                                        let b = p.base_url.trim().to_string();
                                        let e = p.api_key_env.trim().to_string();
                                        if b.is_empty() {
                                            "no base_url".to_string()
                                        } else if e.is_empty() {
                                            b
                                        } else {
                                            format!("{b} · {e}")
                                        }
                                    };

                                    view! {
                                        <div
                                            class=if ci == sel.get() { "ms-row ms-sel" } else { "ms-row" }
                                            on:click=move |_| sel.set(ci)
                                        >
                                            <span
                                                class="ms-card-dot"
                                                class:on=move || reach() == Some(true)
                                                class:unknown=move || reach().is_none() && !pending()
                                                title=move || {
                                                    match reach() {
                                                        Some(true) => detail(),
                                                        Some(false) => format!("unreachable — {}", detail()),
                                                        None if pending() => "testing…".to_string(),
                                                        None => "not tested yet".to_string(),
                                                    }
                                                }
                                            ></span>
                                            <span class="ms-name">{ label }</span>
                                            <span class="ms-sub">{ sub }</span>
                                            <button
                                                class="ms-mini"
                                                title="test this provider again"
                                                disabled=move || pending()
                                                on:click=move |e: MouseEvent| {
                                                    e.stop_propagation();
                                                    probe_cards(Some(ci));
                                                }
                                            >{ "\u{21bb}" }</button>
                                        </div>
                                    }
                                })
                                .collect_view()
                        } }
                            <button
                                class="ns-btn ms-add"
                                on:click=move |_| {
                                    draft.update(|d| {
                                        d.providers.push(ProviderDraft {
                                            base_url: String::new(),
                                            api_key_env: "MODEL_API_KEY".into(),
                                            key_input: String::new(),
                                            models: vec![
                                                ModelDraft { expanded: true, ..Default::default() },
                                            ],
                                        });
                                    });
                                    sel.set(draft.get_untracked().providers.len().saturating_sub(1));
                                }
                            >{ "+ New Provider" }</button>
                        </div>

                        <div class="ms-form">
                            { move || {
                                let ci = sel.get();
                                if ci >= draft.get().providers.len() {
                                    return ().into_any();
                                }
                                // Same key discipline as the rail: the
                                // closures stay `Copy`.
                                let card_of = move || {
                                    draft.get().providers.get(ci).cloned().unwrap_or_default()
                                };
                                let k = move || card_key(&card_of());
                                let fbusy = move || {
                                    fetching.get().get(&k()).copied().unwrap_or(false)
                                };
                                view! {
                                                <TextRow
                                                    label="base_url"
                                                    id="ms-base-url"
                                                    placeholder="http://127.0.0.1:8000"
                                                    value=Signal::derive(move || {
                                                        draft.get().providers.get(ci).map(|p| p.base_url.clone()).unwrap_or_default()
                                                    })
                                                    on_input=Callback::new(move |v: String| {
                                                        draft.update(|d| {
                                                            if let Some(p) = d.providers.get_mut(ci) { p.base_url = v }
                                                        })
                                                    })
                                                />
                                                <label class="ns-label" for="ms-key">{ "api key" }</label>
                                                <div class="ms-keyrow">
                                                    <input
                                                        id="ms-key"
                                                        class="ns-input"
                                                        type="password"
                                                        autocomplete="off"
                                                        placeholder="sk-…"
                                                        prop:value=move || {
                                                            draft.get().providers.get(ci).map(|p| p.key_input.clone()).unwrap_or_default()
                                                        }
                                                        on:input=move |ev| {
                                                            let v = event_target_value(&ev);
                                                            draft.update(|d| {
                                                                if let Some(p) = d.providers.get_mut(ci) { p.key_input = v }
                                                            })
                                                        }
                                                    />
                                                    <button
                                                        class="ns-btn"
                                                        disabled=move || key_busy.get()
                                                        on:click=move |_| save_key(ci)
                                                    >{ "Save key" }</button>
                                                    {
                                                        let stored_name = move || {
                                                            let n = draft
                                                                .get()
                                                                .providers
                                                                .get(ci)
                                                                .map(|p| p.api_key_env.clone())
                                                                .unwrap_or_default();
                                                            if n.trim().is_empty() { "MODEL_API_KEY".to_string() } else { n.trim().to_string() }
                                                        };
                                                        let stored = {
                                                            let stored_name = stored_name;
                                                            move || {
                                                                view.get()
                                                                    .and_then(|v| v.key_env_stored.get(&stored_name()).copied())
                                                                    .unwrap_or(false)
                                                            }
                                                        };
                                                        view! {
                                                            <Show when=stored>
                                                                <button
                                                                    class="ns-btn ns-danger"
                                                                    disabled=move || key_busy.get()
                                                                    on:click=move |_| clear_key(ci)
                                                                >{ "Clear" }</button>
                                                            </Show>
                                                        }
                                                    }
                                                </div>
                                                <TextRow
                                                    label="api_key_env"
                                                    id="ms-key-env"
                                                    placeholder="MODEL_API_KEY"
                                                    value=Signal::derive(move || {
                                                        draft.get().providers.get(ci).map(|p| p.api_key_env.clone()).unwrap_or_default()
                                                    })
                                                    on_input=Callback::new(move |v: String| {
                                                        draft.update(|d| {
                                                            if let Some(p) = d.providers.get_mut(ci) { p.api_key_env = v }
                                                        })
                                                    })
                                                />
                                                <div class="ms-key-hint">
                                                    { move || {
                                                        let raw = draft
                                                            .get()
                                                            .providers
                                                            .get(ci)
                                                            .map(|p| p.api_key_env.clone())
                                                            .unwrap_or_default();
                                                        let name = if raw.trim().is_empty() { "MODEL_API_KEY".to_string() } else { raw.trim().to_string() };
                                                        let v = view.get();
                                                        let present = v.as_ref().and_then(|v| v.key_env_present.get(&name).copied()).unwrap_or(false);
                                                        let stored = v.as_ref().and_then(|v| v.key_env_stored.get(&name).copied()).unwrap_or(false);
                                                        if present && stored { format!("✓ {name} — stored locally") }
                                                        else if present { format!("✓ {name} — from the server environment") }
                                                        else { format!("✗ no key for {name}") }
                                                    } }
                                                </div>

                                                <div class="ms-models">
                                                    { move || {
                                                        draft
                                                            .get()
                                                            .providers
                                                            .get(ci)
                                                            .map(|p| p.models.clone())
                                                            .unwrap_or_default()
                                                            .into_iter()
                                                            .enumerate()
                                                            .map(|(mi, m)| {
                                                                let arrow = if m.expanded { "\u{25be}" } else { "\u{25b8}" };
                                                                let mi_arrow = mi;
                                                                view! {
                                                                    <div class="ms-model">
                                                                        <div class="ms-model-row">
                                                                            <input
                                                                                class="ns-input ms-model-id"
                                                                                placeholder="model_id"
                                                                                prop:value=move || {
                                                                                    draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.model_id.clone()).unwrap_or_default()
                                                                                }
                                                                                on:input=move |ev| {
                                                                                    let v = event_target_value(&ev);
                                                                                    draft.update(|d| {
                                                                                        if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.model_id = v }
                                                                                    })
                                                                                }
                                                                            />
                                                                            <button
                                                                                class="ms-mini"
                                                                                title="details"
                                                                                on:click=move |_| {
                                                                                    draft.update(|d| {
                                                                                        if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi_arrow)) { m.expanded = !m.expanded }
                                                                                    })
                                                                                }
                                                                            >{ arrow }</button>
                                                                            <button
                                                                                class="ms-mini ms-mini-danger"
                                                                                title="remove this model"
                                                                                on:click=move |_| {
                                                                                    draft.update(|d| {
                                                                                        if let Some(p) = d.providers.get_mut(ci) { p.models.remove(mi); }
                                                                                    })
                                                                                }
                                                                            >{ "\u{2715}" }</button>
                                                                        </div>
                                                                        <Show when=move || {
                                                                            draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.expanded).unwrap_or(false)
                                                                        }>
                                                                            <div class="ms-model-details">
                                                                                <TextRow
                                                                                    label="Showed name"
                                                                                    id="ms-name"
                                                                                    placeholder="the name sessions show"
                                                                                    value=Signal::derive(move || {
                                                                                        draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.name.clone()).unwrap_or_default()
                                                                                    })
                                                                                    on_input=Callback::new(move |v: String| {
                                                                                        draft.update(|d| {
                                                                                            if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.name = v }
                                                                                        })
                                                                                    })
                                                                                />
                                                                                <TextRow
                                                                                    label="context_tokens"
                                                                                    id="ms-ctx"
                                                                                    placeholder="262144"
                                                                                    value=Signal::derive(move || {
                                                                                        draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.context_tokens.clone()).unwrap_or_default()
                                                                                    })
                                                                                    on_input=Callback::new(move |v: String| {
                                                                                        draft.update(|d| {
                                                                                            if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.context_tokens = v }
                                                                                        })
                                                                                    })
                                                                                />
                                                                                <TextRow
                                                                                    label="max_output_tokens"
                                                                                    id="ms-max-out"
                                                                                    placeholder="empty = inherit"
                                                                                    value=Signal::derive(move || {
                                                                                        draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.max_output_tokens.clone()).unwrap_or_default()
                                                                                    })
                                                                                    on_input=Callback::new(move |v: String| {
                                                                                        draft.update(|d| {
                                                                                            if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.max_output_tokens = v }
                                                                                        })
                                                                                    })
                                                                                />
                                                                                <label class="ns-label">{ "reasoning_effort" }</label>
                                                                                <div class="ms-seg">
                                                                                    <button
                                                                                        class:on=move || {
                                                                                            draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.reasoning_effort.is_empty()).unwrap_or(true)
                                                                                        }
                                                                                        on:click=move |_| {
                                                                                            draft.update(|d| {
                                                                                                if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.reasoning_effort.clear() }
                                                                                            })
                                                                                        }
                                                                                    >{ "inherit" }</button>
                                                                                    { move || {
                                                                                        let mid = draft
                                                                                            .get()
                                                                                            .providers
                                                                                            .get(ci)
                                                                                            .and_then(|p| p.models.get(mi))
                                                                                            .map(|m| m.model_id.clone())
                                                                                            .unwrap_or_default();
                                                                                        effort_choices(&mid)
                                                                                            .into_iter()
                                                                                            .map(|v| {
                                                                                            let vv = v.to_string();
                                                                                            view! {
                                                                                                <button
                                                                                                    class:on=move || {
                                                                                                        draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.reasoning_effort == vv).unwrap_or(false)
                                                                                                    }
                                                                                                    on:click=move |_| {
                                                                                                        let v = v.to_string();
                                                                                                        draft.update(|d| {
                                                                                                            if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.reasoning_effort = v }
                                                                                                        })
                                                                                                    }
                                                                                                >{ v }</button>
                                                                                            }
                                                                                        })
                                                                                        .collect_view()
                                                                                    } }
                                                                                    <button
                                                                                        class:on=move || {
                                                                                            draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.advanced).unwrap_or(false)
                                                                                        }
                                                                                        title="show every level the kernel knows"
                                                                                        on:click=move |_| {
                                                                                            draft.update(|d| {
                                                                                                if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.advanced = !m.advanced }
                                                                                            })
                                                                                        }
                                                                                    >{ "advanced" }</button>
                                                                                </div>
                                                                                <TextRow
                                                                                    label="timeout_s"
                                                                                    id="ms-timeout"
                                                                                    placeholder="0 = no limit, empty = inherit"
                                                                                    value=Signal::derive(move || {
                                                                                        draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.timeout_s.clone()).unwrap_or_default()
                                                                                    })
                                                                                    on_input=Callback::new(move |v: String| {
                                                                                        draft.update(|d| {
                                                                                            if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.timeout_s = v }
                                                                                        })
                                                                                    })
                                                                                />
                                                                                <TextRow
                                                                                    label="estimate_chars_per_token"
                                                                                    id="ms-cpt"
                                                                                    placeholder="empty = inherit"
                                                                                    value=Signal::derive(move || {
                                                                                        draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.estimate_chars_per_token.clone()).unwrap_or_default()
                                                                                    })
                                                                                    on_input=Callback::new(move |v: String| {
                                                                                        draft.update(|d| {
                                                                                            if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.estimate_chars_per_token = v }
                                                                                        })
                                                                                    })
                                                                                />
                                                                                <label class="ms-check">
                                                                                    <input
                                                                                        type="checkbox"
                                                                                        prop:checked=move || {
                                                                                            draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.vision).unwrap_or(false)
                                                                                        }
                                                                                        on:change=move |ev| {
                                                                                            let v = event_target_checked(&ev);
                                                                                            draft.update(|d| {
                                                                                                if let Some(m) = d.providers.get_mut(ci).and_then(|p| p.models.get_mut(mi)) { m.vision = v }
                                                                                            })
                                                                                        }
                                                                                    />
                                                                                    { "vision" }
                                                                                </label>
                                                                                <Show when=move || {
                                                                                    draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| !m.extra_keys.is_empty()).unwrap_or(false)
                                                                                }>
                                                                                    <div class="ms-extra">
                                                                                        { move || format!(
                                                                                            "kept as they are: {}",
                                                                                            draft.get().providers.get(ci).and_then(|p| p.models.get(mi)).map(|m| m.extra_keys.join(", ")).unwrap_or_default(),
                                                                                        ) }
                                                                                    </div>
                                                                                </Show>
                                                                            </div>
                                                                        </Show>
                                                                    </div>
                                                                }
                                                            })
                                                            .collect_view()
                                                    } }
                                                </div>

                                                <div class="ms-actions">
                                                    <button
                                                        class="ns-btn ms-add"
                                                        on:click=move |_| {
                                                            draft.update(|d| {
                                                                if let Some(p) = d.providers.get_mut(ci) {
                                                                    let used = p.models.iter().filter(|m| m.name.trim().is_empty()).count();
                                                                    p.models.push(ModelDraft {
                                                                        name: String::new(),
                                                                        context_tokens: String::new(),
                                                                        expanded: true,
                                                                        ..Default::default()
                                                                    });
                                                                    let _ = used;
                                                                }
                                                            })
                                                        }
                                                    >{ "+ Model" }</button>
                                                    <button
                                                        class="ns-btn"
                                                        disabled=move || fbusy()
                                                        on:click=move |_| fetch_models(ci)
                                                    >{ move || if fbusy() { "Fetching…" } else { "Fetch models" } }</button>
                                                    <button
                                                        class="ns-btn ns-danger"
                                                        disabled=move || draft.get().providers.len() <= 1
                                                        on:click=move |_| {
                                                            draft.update(|d| {
                                                                if d.providers.len() > 1 {
                                                                    d.providers.remove(ci);
                                                                }
                                                            })
                                                        }
                                                    >{ "Delete provider" }</button>
                                                </div>
                                                <Show when=move || {
                                                    !fetch_err.get().get(&k()).cloned().unwrap_or_default().is_empty()
                                                }>
                                                    <div class="ns-err">
                                                        { move || fetch_err.get().get(&k()).cloned().unwrap_or_default() }
                                                    </div>
                                                </Show>
                                                <Show when=move || {
                                                    !fetched.get().get(&k()).cloned().unwrap_or_default().is_empty()
                                                }>
                                                    <div class="ms-fetched">
                                                        { move || {
                                                            fetched.get().get(&k()).cloned().unwrap_or_default()
                                                                .into_iter()
                                                                .map(|m| {
                                                                    let id = m.id.clone();
                                                                    let label = match m.context_window {
                                                                        Some(w) => format!("{}  (window {w})", m.id),
                                                                        None => m.id.clone(),
                                                                    };
                                                                    let hit = id.clone();
                                                                    let win = m.context_window;
                                                                    view! {
                                                                        <button
                                                                            class="qsel-opt ms-fetched-opt"
                                                                            on:click=move |_| {
                                                                                let id = hit.clone();
                                                                                draft.update(|d| {
                                                                                    if let Some(p) = d.providers.get_mut(ci) {
                                                                                        if p.models.iter().any(|m| m.model_id == id) {
                                                                                            return;
                                                                                        }
                                                                                        p.models.push(ModelDraft {
                                                                                            name: id.clone(),
                                                                                            model_id: id.clone(),
                                                                                            context_tokens: win.map(|w| w.to_string()).unwrap_or_default(),
                                                                                            expanded: false,
                                                                                            ..Default::default()
                                                                                        });
                                                                                    }
                                                                                });
                                                                            }
                                                                        >
                                                                            <span class="qsel-tick">{ "+" }</span>
                                                                            { label }
                                                                        </button>
                                                                    }
                                                                })
                                                                .collect_view()
                                                        } }
                                                    </div>
                                                </Show>
                                }.into_any()
                            } }
                        </div>
                    </div>

                    <Show when=move || saved.get().is_some()>
                        <div class="ms-banner">
                            { move || {
                                let v = saved.get().unwrap_or(Value::Null);
                                let mut lines = vec!["Saved.".to_string()];
                                if let Some(r) = v.get("needs_loop_restart").and_then(|x| x.as_array()) {
                                    if !r.is_empty() {
                                        lines.push("Restart the session's loop for these to take full effect:".to_string());
                                        for x in r {
                                            if let Some(s) = x.as_str() {
                                                lines.push(format!("· {s}"));
                                            }
                                        }
                                    }
                                }
                                if let Some(w) = v.get("warnings").and_then(|x| x.as_array()) {
                                    for x in w {
                                        if let Some(s) = x.as_str() {
                                            lines.push(format!("⚠ {s}"));
                                        }
                                    }
                                }
                                let backup = v
                                    .get("backup")
                                    .and_then(|x| x.as_str())
                                    .map(|b| format!("backup: {b}"))
                                    .unwrap_or_default();
                                if !backup.is_empty() {
                                    lines.push(backup);
                                }
                                lines.join("\n")
                            } }
                        </div>
                    </Show>

                    <Show when=move || key_msg.get().is_some()>
                        <div class="ms-key-hint">{ move || key_msg.get().unwrap_or_default() }</div>
                    </Show>

                    <Show when=move || {
                        view.get().map(|v| !v.notes.is_empty()).unwrap_or(false)
                    }>
                        <div class="ms-notes">
                            { move || {
                                view.get()
                                    .map(|v| v.notes.join("\n"))
                                    .unwrap_or_default()
                            } }
                        </div>
                    </Show>

                    <div class="ns-err">{ move || err.get().unwrap_or_default() }</div>

                    <div class="ns-actions">
                        <button class="ns-btn ns-cancel" on:click=move |_| close()>{ "Close" }</button>
                        <button
                            class="ns-btn"
                            disabled=move || busy.get()
                            on:click=move |_| reload()
                        >{ "Reload" }</button>
                        <button
                            class="ns-btn ns-create"
                            disabled=move || busy.get()
                            on:click=move |_| save()
                        >{ if busy.get() { "Saving…" } else { "Save" } }</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
