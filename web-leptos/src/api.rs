//! REST helpers against the rushi-web server (port of the JS fetch calls).

use gloo_net::http::Request;
use serde::Serialize;
use serde_json::{json, Value};

async fn post_json(url: &str, body: &impl Serialize) -> Result<u16, String> {
    let payload = serde_json::to_string(body).map_err(|e| e.to_string())?;
    let req = Request::post(url)
        .header("Content-Type", "application/json")
        .body(payload.as_str())
        .map_err(|e| e.to_string())?;
    let res = req.send().await.map_err(|e| e.to_string())?;
    Ok(res.status())
}

pub async fn load_sessions() -> Result<Vec<crate::model::SessionInfo>, String> {
    let res = Request::get("/api/sessions").send().await.map_err(|e| e.to_string())?;
    let text = res.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Create a session, optionally with a chosen working directory, model
/// entry and reasoning effort (v0.5.45: the new-session form picks the
/// model, so the model panel no longer has to expose a global default).
pub async fn create_session(
    name: &str,
    cwd: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), String> {
    let status = post_json(
        "/api/sessions",
        &json!({ "name": name, "cwd": cwd, "model": model, "effort": effort }),
    )
    .await?;
    if status >= 400 {
        Err(format!("create failed: HTTP {status}"))
    } else {
        Ok(())
    }
}

// ── model settings (v0.5.42) ────────────────────────────────────────

/// The server's model-config snapshot (parsed out of `config.toml`).
pub async fn load_model() -> Result<crate::model::ModelSettingsView, String> {
    let res = Request::get("/api/model").send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    let text = res.text().await.map_err(|e| e.to_string())?;
    if status >= 400 {
        return Err(text);
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Write the edited settings back. Returns the server's outcome (which
/// changes need a loop restart) or the server's validation message.
pub async fn save_model(view: &crate::model::ModelSettingsView) -> Result<Value, String> {
    let payload = serde_json::to_string(view).map_err(|e| e.to_string())?;
    let req = Request::post("/api/model")
        .header("Content-Type", "application/json")
        .body(payload)
        .map_err(|e| e.to_string())?;
    let res = req.send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    let text = res.text().await.map_err(|e| e.to_string())?;
    if status >= 400 {
        return Err(text);
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Probe a candidate provider endpoint without saving anything.
pub async fn probe_model(
    base_url: &str,
    api_key_env: Option<&str>,
    model_id: Option<&str>,
) -> Result<Value, String> {
    let payload = json!({
        "base_url": base_url,
        "api_key_env": api_key_env,
        "model_id": model_id,
    });
    let req = Request::post("/api/model/probe")
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .map_err(|e| e.to_string())?;
    let res = req.send().await.map_err(|e| e.to_string())?;
    let text = res.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// v0.5.44: set (or clear, with `None`) a session's model entry. The
/// choice takes effect the next time that session's loop starts.
pub async fn set_session_model(session: &str, model: Option<&str>) -> Result<(), String> {
    let url = format!("/api/sessions/{}/model", js_sys::encode_uri_component(session));
    let payload = serde_json::json!({ "model": model });
    let req = Request::post(&url)
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .map_err(|e| e.to_string())?;
    let res = req.send().await.map_err(|e| e.to_string())?;
    if res.status() >= 400 {
        return Err(res.text().await.unwrap_or_default());
    }
    Ok(())
}

/// v0.5.44: store (or, with `None`, clear) the key for one env var name.
/// The key is never read back — only its presence is reported.
pub async fn set_model_key(name: &str, value: Option<&str>) -> Result<(), String> {
    let payload = json!({ "name": name, "value": value });
    let req = Request::post("/api/model/key")
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .map_err(|e| e.to_string())?;
    let res = req.send().await.map_err(|e| e.to_string())?;
    if res.status() >= 400 {
        return Err(res.text().await.unwrap_or_default());
    }
    Ok(())
}

/// The server's default working directory (prefill for the picker).
pub async fn default_cwd() -> Option<String> {
    let res = Request::get("/api/default-cwd").send().await.ok()?;
    let text = res.text().await.ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(|s| s.to_string()))
}

/// A directory listing for the new-session directory picker.
#[derive(Clone, Debug, Default)]
pub struct BrowseResult {
    pub path: String,
    pub parent: Option<String>,
    pub dirs: Vec<String>,
    pub error: Option<String>,
}

pub async fn browse(path: Option<&str>) -> Result<BrowseResult, String> {
    let url = match path {
        Some(p) if !p.is_empty() => {
            let enc: String = js_sys::encode_uri_component(p).into();
            format!("/api/browse?path={enc}")
        }
        _ => "/api/browse".to_string(),
    };
    let res = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    let text = res.text().await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(BrowseResult {
        path: v.get("path").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        parent: v.get("parent").and_then(|x| x.as_str()).map(|s| s.to_string()),
        dirs: v
            .get("dirs")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        error: v.get("error").and_then(|x| x.as_str()).map(|s| s.to_string()),
    })
}

// Kept for API parity with the legacy JS; initial history arrives over
// the WS "history" replay instead of this endpoint.
#[allow(dead_code)]
pub async fn load_events(id: &str) -> Result<Vec<Value>, String> {
    let res = Request::get(&format!("/api/sessions/{}/events", id))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let text = res.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

pub async fn rename_session(old: &str, name: &str) -> Result<(), String> {
    let status = post_json(
        &format!("/api/sessions/{}/rename", old),
        &json!({ "name": name }),
    )
    .await?;
    if status >= 400 {
        Err(format!("rename failed: HTTP {status}"))
    } else {
        Ok(())
    }
}

pub async fn delete_session(name: &str) -> Result<(), String> {
    let req = Request::delete(&format!("/api/sessions/{name}"));
    let res = req.send().await.map_err(|e| e.to_string())?;
    if res.status() >= 400 {
        Err(format!("delete failed: HTTP {}", res.status()))
    } else {
        Ok(())
    }
}

pub async fn start_loop(id: &str) -> Result<(), String> {
    let status = post_json(&format!("/api/sessions/{id}/start"), &json!({})).await?;
    if status >= 400 {
        Err(format!("start failed: HTTP {status}"))
    } else {
        Ok(())
    }
}

pub async fn stop_loop(id: &str) -> Result<(), String> {
    post_json(&format!("/api/sessions/{id}/stop"), &json!({})).await?;
    // 404 body says "no running loop" — treat as success for the UI flag.
    Ok(())
}

pub async fn loop_running(id: &str) -> bool {
    let Ok(res) = Request::get(&format!("/api/sessions/{id}/loop"))
        .send()
        .await
    else {
        return false;
    };
    let Ok(text) = res.text().await else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("running").and_then(|r| r.as_bool()))
        .unwrap_or(false)
}

/// v0.5.55 P1: the server's full sidebar loop state. `running` = sessions
/// with a live loop process (orange breathing lamp); `finished` = sessions
/// whose most recent loop ended and left a `loop.last` marker but are not
/// currently running (green "finished, unviewed" lamp). The client polls
/// this on load and on a timer so both lamp kinds stay correct even with
/// no live WS connection open (a fresh page load has no active session,
/// hence no WS, so the connect-time `loops` frame never arrives).
pub struct LoopsSnapshot {
    pub running: Vec<String>,
    pub finished: Vec<String>,
}

impl Default for LoopsSnapshot {
    fn default() -> Self {
        Self {
            running: Vec::new(),
            finished: Vec::new(),
        }
    }
}

pub async fn load_loops() -> LoopsSnapshot {
    let Ok(res) = Request::get("/api/loops").send().await else {
        return LoopsSnapshot::default();
    };
    let Ok(text) = res.text().await else {
        return LoopsSnapshot::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return LoopsSnapshot::default();
    };
    let running = v
        .get("running")
        .and_then(|r| r.as_array())
        .cloned()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let finished = v
        .get("finished")
        .and_then(|f| f.as_array())
        .cloned()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("session").and_then(|s| s.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    LoopsSnapshot { running, finished }
}

/// v0.5.55 P1: the user viewed this session, so its finished-unviewed
/// green lamp is consumed. Fire-and-forget; the server deletes the
/// `loop.last` marker so the next poll does not re-light the lamp.
pub async fn mark_loop_viewed(id: &str) {
    let _ = Request::post(&format!("/api/sessions/{id}/loop/viewed")).send().await;
}

pub async fn load_goal(id: &str) -> Option<crate::model::GoalView> {
    let res = Request::get(&format!("/api/sessions/{id}/goal")).send().await.ok()?;
    let text = res.text().await.ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| serde_json::from_value(v.get("current")?.clone()).ok())
}

pub async fn goal_action(id: &str, action: &str, goal: Option<&str>) {
    let payload = json!({
        "action": action,
        "goal": goal,
    });
    let _ = post_json(&format!("/api/sessions/{id}/goal"), &payload).await;
}

/// v0.5.54: the session's essence entries (invariants + beliefs) for the
/// sidebar essence plugin. Read-only — the harness's `essence` tool is the
/// writer; the server endpoint degrades to an empty list when the store is
/// absent or mid-write.
pub async fn load_essence(id: &str) -> Result<Vec<crate::model::EssenceEntry>, String> {
    let res = Request::get(&format!("/api/sessions/{id}/essence"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let text = res.text().await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    serde_json::from_value(
        v.get("entries").cloned().unwrap_or(Value::Array(vec![])),
    )
    .map_err(|e| e.to_string())
}

/// Rewind plugin: the session's projected history tree (read-only). The
/// server degrades a missing log to an empty tree.
pub async fn load_rewind_tree(id: &str) -> Result<crate::model::RewindTree, String> {
    let res = Request::get(&format!("/api/sessions/{id}/rewind"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let text = res.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Rewind plugin: write a `rewind` marker. `mode:"on"` keeps the target user
/// message as the active tail and abandons everything after it; the fork
/// stays in the log and can be re-entered later.
pub async fn post_rewind(id: &str, target_seq: u64, mode: &str) -> Result<(), String> {
    let payload = json!({ "target_seq": target_seq, "mode": mode });
    let status = post_json(&format!("/api/sessions/{id}/rewind"), &payload).await?;
    if status >= 400 {
        Err(format!("rewind failed: HTTP {status}"))
    } else {
        Ok(())
    }
}

pub async fn post_message(
    id: &str,
    content: &str,
    queue: Option<&str>,
) -> Result<(), String> {
    let payload = json!({
        "content": content,
        "queue": queue,
    });
    let status = post_json(&format!("/api/sessions/{id}/messages"), &payload).await?;
    if status >= 400 {
        Err(format!("send failed: HTTP {status}"))
    } else {
        Ok(())
    }
}

// ── M8: right-panel Files endpoints (cwd-escape guarded server-side) ──

fn files_qparam(key: &str, val: &str) -> String {
    format!("{key}={}", js_sys::encode_uri_component(val))
}

fn files_error(status: u16, text: &str) -> String {
    match status {
        403 => format!("forbidden: {text}"),
        404 => format!("not found: {text}"),
        413 => format!("too large: {text}"),
        415 => format!("unsupported: {text}"),
        _ => format!("HTTP {status}: {text}"),
    }
}

/// M8: one directory of the active session's workdir for the Files
/// tree. `path` is workdir-relative ("" = the workdir root).
pub async fn list_dir(session: &str, path: &str) -> Result<crate::model::DirList, String> {
    let url = format!(
        "/api/files?{}",
        if path.is_empty() {
            files_qparam("session", session)
        } else {
            format!("{}&{}", files_qparam("session", session), files_qparam("path", path))
        }
    );
    let res = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if res.status() >= 400 {
        let body = res.text().await.unwrap_or_default();
        return Err(files_error(res.status(), &body));
    }
    let text = res.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// M8: text/code content for the preview pane (512 KiB cap, first
/// chunk of larger files with `truncated: true`; binary → Err).
pub async fn read_file(
    session: &str,
    path: &str,
) -> Result<crate::model::FilePreview, String> {
    let url = format!(
        "/api/file?{}&{}",
        files_qparam("session", session),
        files_qparam("path", path)
    );
    let res = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if res.status() >= 400 {
        let body = res.text().await.unwrap_or_default();
        return Err(files_error(res.status(), &body));
    }
    let text = res.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// M8: the `<img src>` for an image preview — the server streams the
/// raw bytes with the right Content-Type.
pub fn raw_url(session: &str, path: &str) -> String {
    format!(
        "/api/raw?{}&{}",
        files_qparam("session", session),
        files_qparam("path", path)
    )
}
