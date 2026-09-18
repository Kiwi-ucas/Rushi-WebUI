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

/// Create a session, optionally with a chosen working directory.
pub async fn create_session(name: &str, cwd: Option<&str>) -> Result<(), String> {
    let status = post_json("/api/sessions", &json!({ "name": name, "cwd": cwd })).await?;
    if status >= 400 {
        Err(format!("create failed: HTTP {status}"))
    } else {
        Ok(())
    }
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
