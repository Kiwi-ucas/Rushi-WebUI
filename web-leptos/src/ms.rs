//! Model settings panel (v0.5.42).
//!
//! Edits the kernel `config.toml`'s model section through the server's
//! `/api/model` endpoints: the global `[model]` defaults, one
//! `[model."<name>"]` entry per provider, and `[active] model`. The
//! server owns validation and the file write; this panel only builds the
//! draft and renders what came back.
//!
//! Everything numeric is edited as text (`""` = inherit / drop the key),
//! because the kernel silently falls back to a default on a type
//! mismatch — an empty box meaning "leave it unset" is clearer than a
//! zero.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;
use web_sys::MouseEvent;

use crate::api;
use crate::model::{AppState, Globals, ModelEntry, ModelSettingsView};
use crate::ui::after_dispatch;

const EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Text draft of one entry. Empty string = key absent.
#[derive(Clone, Debug, Default, PartialEq)]
struct EntryDraft {
    name: String,
    model_id: String,
    base_url: String,
    api_key_env: String,
    context_tokens: String,
    max_output_tokens: String,
    reasoning_effort: String,
    vision: bool,
    timeout_s: String,
    estimate_chars_per_token: String,
    extra_keys: Vec<String>,
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

impl EntryDraft {
    fn from_entry(e: &ModelEntry) -> Self {
        Self {
            name: e.name.clone(),
            model_id: s(&e.model_id),
            base_url: s(&e.base_url),
            api_key_env: s(&e.api_key_env),
            context_tokens: n(&e.context_tokens),
            max_output_tokens: n(&e.max_output_tokens),
            reasoning_effort: s(&e.reasoning_effort),
            vision: e.vision == Some(true),
            timeout_s: n(&e.timeout_s),
            estimate_chars_per_token: n(&e.estimate_chars_per_token),
            extra_keys: e.extra_keys.clone(),
        }
    }

    fn to_entry(&self) -> ModelEntry {
        let opt = |v: &str| {
            let t = v.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        ModelEntry {
            name: self.name.trim().to_string(),
            model_id: opt(&self.model_id),
            base_url: opt(&self.base_url),
            api_key_env: opt(&self.api_key_env),
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

#[derive(Clone, Debug, Default, PartialEq)]
struct GlobalsDraft {
    max_output_tokens: String,
    reasoning_effort: String,
    model_timeout_s: String,
    estimate_chars_per_token: String,
    vision: bool,
}

impl GlobalsDraft {
    fn from_globals(g: &Globals) -> Self {
        Self {
            max_output_tokens: n(&g.max_output_tokens),
            reasoning_effort: s(&g.reasoning_effort),
            model_timeout_s: n(&g.model_timeout_s),
            estimate_chars_per_token: n(&g.estimate_chars_per_token),
            vision: g.vision == Some(true),
        }
    }

    fn to_globals(&self) -> Globals {
        let opt = |v: &str| {
            let t = v.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        Globals {
            max_output_tokens: parse_n(&self.max_output_tokens),
            reasoning_effort: opt(&self.reasoning_effort),
            model_timeout_s: parse_n(&self.model_timeout_s),
            estimate_chars_per_token: parse_n(&self.estimate_chars_per_token),
            vision: if self.vision { Some(true) } else { None },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Draft {
    active: String,
    sel: usize,
    entries: Vec<EntryDraft>,
    globals: GlobalsDraft,
}

impl Draft {
    fn from_view(v: &ModelSettingsView) -> Self {
        let entries: Vec<EntryDraft> =
            v.entries.iter().map(EntryDraft::from_entry).collect();
        let sel = v
            .entries
            .iter()
            .position(|e| e.name == v.active)
            .unwrap_or(0);
        Self {
            active: v.active.clone(),
            sel,
            entries,
            globals: GlobalsDraft::from_globals(&v.globals),
        }
    }
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
    let probes = state.model_probe;
    let draft = RwSignal::new(Draft::default());

    let reload = move || {
        err.set(None);
        saved.set(None);
        spawn_local(async move {
            match api::load_model().await {
                Ok(v) => {
                    draft.set(Draft::from_view(&v));
                    view.set(Some(v));
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

    let cur = move || draft.get().entries.get(draft.get().sel).cloned();
    let sel = move || draft.get().sel;

    let save = move || {
        let Some(v) = view.get() else { return };
        let d = draft.get();
        let payload = ModelSettingsView {
            active: d.active.clone(),
            entries: d.entries.iter().map(|e| e.to_entry()).collect(),
            globals: d.globals.to_globals(),
            config_path: v.config_path.clone(),
            mirror_path: v.mirror_path.clone(),
            key_env_present: v.key_env_present.clone(),
            effective: v.effective.clone(),
            notes: v.notes.clone(),
        };
        busy.set(true);
        err.set(None);
        spawn_local(async move {
            match api::save_model(&payload).await {
                Ok(out) => {
                    saved.set(Some(out));
                    // Re-read so the panel shows the file as written.
                    if let Ok(fresh) = api::load_model().await {
                        draft.set(Draft::from_view(&fresh));
                        view.set(Some(fresh));
                    }
                }
                Err(e) => err.set(Some(e)),
            }
            busy.set(false);
        });
    };

    let probe = move |idx: usize| {
        let entries = draft.get_untracked().entries;
        let Some(e) = entries.get(idx) else { return };
        let name = e.name.clone();
        let base = e.base_url.trim().to_string();
        let env = e.api_key_env.trim().to_string();
        let mid = e.model_id.trim().to_string();
        let env_opt = (!env.is_empty()).then(|| env.clone());
        let mid_opt = (!mid.is_empty()).then(|| mid.clone());
        if base.is_empty() {
            probes.update(|m| {
                m.insert(
                    name,
                    serde_json::json!({ "ok": false, "detail": "base_url 为空" }),
                );
            });
            return;
        }
        probes.update(|m| {
            m.insert(name.clone(), serde_json::json!({ "pending": true }));
        });
        spawn_local(async move {
            let r = api::probe_model(&base, env_opt.as_deref(), mid_opt.as_deref())
                .await
                .unwrap_or_else(|e| serde_json::json!({ "ok": false, "detail": e }));
            probes.update(|m| {
                m.insert(name, r);
            });
        });
    };

    view! {
        <Show when=move || open.get() fallback=|| ()>
            <div id="ms-backdrop" on:click=move |_| close()>
                <div id="ms-dialog" on:click=move |e: MouseEvent| e.stop_propagation()>
                    <div class="ms-head">
                        <span class="ns-title">{ "模型设置 Model settings" }</span>
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
                                    .entries
                                    .iter()
                                    .enumerate()
                                    .map(|(i, e)| {
                                        let is_sel = i == sel();
                                        let is_active = draft.get().active == e.name;
                                        view! {
                                            <div
                                                class=if is_sel { "ms-row ms-sel" } else { "ms-row" }
                                                on:click=move |_| draft.update(|d| d.sel = i)
                                            >
                                                <span class="ms-dot" class:on=is_active></span>
                                                <span class="ms-name">{ e.name.clone() }</span>
                                                <span class="ms-sub">{ e.base_url.clone() }</span>
                                            </div>
                                        }
                                    })
                                    .collect_view()
                            } }
                            <button
                                class="ns-btn ms-add"
                                on:click=move |_| {
                                    draft
                                        .update(|d| {
                                            d.entries
                                                .push(EntryDraft {
                                                    name: format!("model-{}", d.entries.len() + 1),
                                                    base_url: "http://127.0.0.1:8000".into(),
                                                    api_key_env: "MODEL_API_KEY".into(),
                                                    ..Default::default()
                                                });
                                            d.sel = d.entries.len() - 1;
                                        })
                                }
                            >{ "+ 新增条目" }</button>
                        </div>

                        <div class="ms-form">
                            <Show when=move || cur().is_some() fallback=|| ()>
                                <TextRow
                                    label="名称 name"
                                    id="ms-name"
                                    placeholder="entry name"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.name).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) { e.name = v }
                                            })
                                    })
                                />
                                <TextRow
                                    label="model_id"
                                    id="ms-model-id"
                                    placeholder="provider 侧的模型名（留空=条目名）"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.model_id).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                    e.model_id = v
                                                }
                                            })
                                    })
                                />
                                <TextRow
                                    label="base_url"
                                    id="ms-base-url"
                                    placeholder="http://127.0.0.1:8000"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.base_url).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                    e.base_url = v
                                                }
                                            })
                                    })
                                />
                                <TextRow
                                    label="api_key_env（环境变量名，不填 key 本身）"
                                    id="ms-key-env"
                                    placeholder="LLAMA_API_KEY"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.api_key_env).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                    e.api_key_env = v
                                                }
                                            })
                                    })
                                />
                                <div class="ms-key-hint">
                                    { move || {
                                        let name = cur().map(|e| e.api_key_env).unwrap_or_default();
                                        let name = if name.trim().is_empty() {
                                            "MODEL_API_KEY".to_string()
                                        } else {
                                            name.trim().to_string()
                                        };
                                        let present = view
                                            .get()
                                            .and_then(|v| v.key_env_present.get(&name).copied())
                                            .unwrap_or(false);
                                        if present {
                                            format!("✓ {name} 在服务进程里存在")
                                        } else {
                                            format!("✗ {name} 在服务进程里不存在（key 会以空串发出）")
                                        }
                                    } }
                                </div>
                                <TextRow
                                    label="context_tokens（改这个要重启 loop）"
                                    id="ms-ctx"
                                    placeholder="262144"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.context_tokens).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                    e.context_tokens = v
                                                }
                                            })
                                    })
                                />
                                <TextRow
                                    label="max_output_tokens（留空=继承全局）"
                                    id="ms-max-out"
                                    placeholder="32768"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.max_output_tokens).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                    e.max_output_tokens = v
                                                }
                                            })
                                    })
                                />
                                <label class="ns-label">{ "reasoning_effort" }</label>
                                <div class="ms-seg">
                                    <button
                                        class:on=move || cur().map(|e| e.reasoning_effort.is_empty()).unwrap_or(true)
                                        on:click=move |_| {
                                            draft
                                                .update(|d| {
                                                    if let Some(e) = d.entries.get_mut(d.sel) {
                                                        e.reasoning_effort.clear()
                                                    }
                                                })
                                        }
                                    >{ "inherit" }</button>
                                    { EFFORTS
                                        .iter()
                                        .map(|v| {
                                            let v = v.to_string();
                                            let vv = v.clone();
                                            view! {
                                                <button
                                                    class:on=move || {
                                                        cur().map(|e| e.reasoning_effort == vv).unwrap_or(false)
                                                    }
                                                    on:click=move |_| {
                                                        let v = v.clone();
                                                        draft
                                                            .update(|d| {
                                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                                    e.reasoning_effort = v
                                                                }
                                                            })
                                                    }
                                                >{ v.clone() }</button>
                                            }
                                        })
                                        .collect_view() }
                                </div>
                                <TextRow
                                    label="timeout_s（0=无上限，留空=继承全局）"
                                    id="ms-timeout"
                                    placeholder="3600"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.timeout_s).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                    e.timeout_s = v
                                                }
                                            })
                                    })
                                />
                                <TextRow
                                    label="estimate_chars_per_token（留空=继承全局）"
                                    id="ms-cpt"
                                    placeholder="4"
                                    value=Signal::derive(move || {
                                        cur().map(|e| e.estimate_chars_per_token).unwrap_or_default()
                                    })
                                    on_input=Callback::new(move |v: String| {
                                        draft
                                            .update(|d| {
                                                if let Some(e) = d.entries.get_mut(d.sel) {
                                                    e.estimate_chars_per_token = v
                                                }
                                            })
                                    })
                                />
                                <label class="ms-check">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || cur().map(|e| e.vision).unwrap_or(false)
                                        on:change=move |ev| {
                                            let v = event_target_checked(&ev);
                                            draft
                                                .update(|d| {
                                                    if let Some(e) = d.entries.get_mut(d.sel) {
                                                        e.vision = v
                                                    }
                                                })
                                        }
                                    />
                                    { "vision（该模型支持图片输入）" }
                                </label>
                                <Show when=move || cur().map(|e| !e.extra_keys.is_empty()).unwrap_or(false)>
                                    <div class="ms-extra">
                                        { move || format!(
                                            "面板不编辑的键（原样保留）：{}",
                                            cur().map(|e| e.extra_keys.join(", ")).unwrap_or_default(),
                                        ) }
                                    </div>
                                </Show>

                                <div class="ms-actions">
                                    <button
                                        class="ns-btn"
                                        disabled=move || busy.get()
                                        on:click=move |_| probe(sel())
                                    >{ "测试连接" }</button>
                                    <button
                                        class="ns-btn ns-danger"
                                        disabled=move || draft.get().entries.len() <= 1
                                        on:click=move |_| {
                                            draft
                                                .update(|d| {
                                                    if d.entries.len() > 1 {
                                                        d.entries.remove(d.sel);
                                                        d.sel = d.sel.min(d.entries.len() - 1);
                                                    }
                                                })
                                        }
                                    >{ "删除该条目" }</button>
                                </div>
                                <Show when=move || probes.get().contains_key(&cur().map(|e| e.name).unwrap_or_default())>
                                    { move || {
                                        let name = cur().map(|e| e.name).unwrap_or_default();
                                        let r = probes.get().get(&name).cloned().unwrap_or(Value::Null);
                                        render_probe(&r)
                                    } }
                                </Show>
                            </Show>
                        </div>
                    </div>

                    <div class="ms-active">
                        <span class="ns-label">{ "活跃模型 active（切换后需重启会话 loop）" }</span>
                        <div class="ms-seg">
                            { move || {
                                draft
                                    .get()
                                    .entries
                                    .iter()
                                    .map(|e| {
                                        let name = e.name.clone();
                                        let n2 = name.clone();
                                        view! {
                                            <button
                                                class:on=move || draft.get().active == n2
                                                on:click=move |_| {
                                                    let name = name.clone();
                                                    draft.update(|d| d.active = name);
                                                }
                                            >{ e.name.clone() }</button>
                                        }
                                    })
                                    .collect_view()
                            } }
                        </div>
                    </div>

                    <div class="ms-globals">
                        <span class="ns-label">{ "全局默认 [model]（只对这 5 个键生效）" }</span>
                        <TextRow
                            label="max_output_tokens"
                            id="ms-g-max"
                            placeholder="32768"
                            value=Signal::derive(move || draft.get().globals.max_output_tokens)
                            on_input=Callback::new(move |v: String| {
                                draft.update(|d| d.globals.max_output_tokens = v)
                            })
                        />
                        <label class="ns-label">{ "reasoning_effort" }</label>
                        <div class="ms-seg">
                            <button
                                class:on=move || draft.get().globals.reasoning_effort.is_empty()
                                on:click=move |_| draft.update(|d| d.globals.reasoning_effort.clear())
                            >{ "unset" }</button>
                            { EFFORTS
                                .iter()
                                .map(|v| {
                                    let v = v.to_string();
                                    let vv = v.clone();
                                    view! {
                                        <button
                                            class:on=move || draft.get().globals.reasoning_effort == vv
                                            on:click=move |_| {
                                                let v = v.clone();
                                                draft.update(|d| d.globals.reasoning_effort = v)
                                            }
                                        >{ v.clone() }</button>
                                    }
                                })
                                .collect_view() }
                        </div>
                        <TextRow
                            label="model_timeout_s"
                            id="ms-g-timeout"
                            placeholder="3600"
                            value=Signal::derive(move || draft.get().globals.model_timeout_s)
                            on_input=Callback::new(move |v: String| {
                                draft.update(|d| d.globals.model_timeout_s = v)
                            })
                        />
                        <TextRow
                            label="estimate_chars_per_token"
                            id="ms-g-cpt"
                            placeholder="4"
                            value=Signal::derive(move || draft.get().globals.estimate_chars_per_token)
                            on_input=Callback::new(move |v: String| {
                                draft.update(|d| d.globals.estimate_chars_per_token = v)
                            })
                        />
                        <label class="ms-check">
                            <input
                                type="checkbox"
                                prop:checked=move || draft.get().globals.vision
                                on:change=move |ev| {
                                    let v = event_target_checked(&ev);
                                    draft.update(|d| d.globals.vision = v)
                                }
                            />
                            { "vision" }
                        </label>
                    </div>

                    <Show when=move || saved.get().is_some()>
                        <div class="ms-banner">
                            { move || {
                                let v = saved.get().unwrap_or(Value::Null);
                                let mut lines = vec!["已保存。".to_string()];
                                if let Some(r) = v.get("needs_loop_restart").and_then(|x| x.as_array()) {
                                    if !r.is_empty() {
                                        lines.push("需重启会话 loop 才完全生效：".to_string());
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
                                    .map(|b| format!("备份：{b}"))
                                    .unwrap_or_default();
                                if !backup.is_empty() {
                                    lines.push(backup);
                                }
                                lines.join("\n")
                            } }
                        </div>
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

                    <Show when=move || view.get().and_then(|v| v.effective).is_some()>
                        <div class="ms-effective">
                            { move || {
                                let e = view.get().and_then(|v| v.effective).unwrap_or(Value::Null);
                                format!(
                                    "当前生效：{} · {} · effort {} · thinking L{}",
                                    e.get("active").and_then(|x| x.as_str()).unwrap_or("-"),
                                    e.get("model_id").and_then(|x| x.as_str()).unwrap_or("-"),
                                    e.get("reasoning_effort").and_then(|x| x.as_str()).unwrap_or("-"),
                                    e.get("thinking_level").and_then(|x| x.as_i64()).unwrap_or(0),
                                )
                            } }
                        </div>
                    </Show>

                    <div class="ns-err">{ move || err.get().unwrap_or_default() }</div>

                    <div class="ns-actions">
                        <button class="ns-btn ns-cancel" on:click=move |_| close()>{ "关闭" }</button>
                        <button
                            class="ns-btn"
                            disabled=move || busy.get()
                            on:click=move |_| reload()
                        >{ "重新载入" }</button>
                        <button
                            class="ns-btn ns-create"
                            disabled=move || busy.get()
                            on:click=move |_| save()
                        >{ if busy.get() { "保存中…" } else { "保存" } }</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

fn render_probe(r: &Value) -> AnyView {
    if r.get("pending").and_then(|x| x.as_bool()).unwrap_or(false) {
        return view! { <div class="ms-test">"测试中…"</div> }.into_any();
    }
    let ok = r.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
    let endpoint = r.get("endpoint").and_then(|x| x.as_str()).unwrap_or("");
    let status = r.get("status").and_then(|x| x.as_u64());
    let detail = r.get("detail").and_then(|x| x.as_str()).unwrap_or("");
    let head = match (ok, status) {
        (true, Some(s)) => format!("✓ 通（{endpoint} HTTP {s}）"),
        (true, None) => format!("✓ 通（{endpoint}）"),
        (false, Some(s)) => format!("✗ 失败（{endpoint} HTTP {s}）"),
        (false, None) => "✗ 失败".to_string(),
    };
    let class = if ok { "ms-test ms-ok" } else { "ms-test ms-fail" };
    view! {
        <div class=class>
            <b>{ head }</b>
            <div class="ms-detail">{ detail.to_string() }</div>
        </div>
    }
    .into_any()
}
