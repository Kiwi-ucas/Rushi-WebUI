use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::Json;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

mod config;
mod goal;
mod process;
mod sessions;

use config::WebConfig;
use process::LoopManager;
use sessions::SessionManager;

// ── Embedded frontend (Phase 5: single-binary distribution) ─────────
mod assets {
    use rust_embed::RustEmbed;

    /// The Leptos/Trunk build output (wasm bundle). In debug mode rust-embed
    /// reads this folder from disk, so a `trunk build` is picked up without
    /// recompiling rushi-web. A placeholder index.html is checked in so the
    /// folder always exists.
    #[derive(RustEmbed)]
    #[folder = "../../web-leptos/dist/"]
    pub struct Frontend;
}

#[derive(Clone)]
struct AppState {
    cfg: Arc<WebConfig>,
    sessions: Arc<SessionManager>,
    loops: Arc<LoopManager>,
}

#[derive(Deserialize)]
struct PostMessage {
    content: String,
    #[serde(default)]
    queue: Option<String>,
}

#[derive(Deserialize)]
struct PostApproval {
    id: String,
    decision: String,
}

#[derive(Deserialize)]
struct PostRewind {
    target_seq: u64,
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "before".into()
}

// ── REST handlers ──────────────────────────────────────────────────

async fn list_sessions(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.sessions.list().await)
}

#[derive(Deserialize)]
struct PostSession {
    name: String,
    #[serde(default)]
    cwd: Option<String>,
}

/// Create a session, optionally with a chosen working directory.
async fn create_session(
    State(st): State<AppState>,
    Json(body): Json<PostSession>,
) -> impl IntoResponse {
    let name = body.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty session name".to_string()).into_response();
    }
    match st.sessions.create(name, body.cwd.as_deref()).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// The default working directory (what a session uses when no `.cwd`
/// marker was recorded) — the sessions root's parent.
fn default_workdir(cfg: &WebConfig) -> PathBuf {
    cfg.sessions_root
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn get_default_cwd(State(st): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({ "cwd": default_workdir(&st.cfg).to_string_lossy() }))
}

#[derive(Deserialize)]
struct BrowseQuery {
    path: Option<String>,
}

/// List the subdirectories of `path` for the new-session directory
/// picker (browsers cannot return a real absolute path from a native
/// directory input, so the server drives the browsing).
async fn browse_dirs(
    State(st): State<AppState>,
    Query(q): Query<BrowseQuery>,
) -> impl IntoResponse {
    let path = q
        .path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_workdir(&st.cfg));
    let path = path.canonicalize().unwrap_or(path);

    let mut dirs = Vec::new();
    let mut err: Option<String> = None;
    match std::fs::read_dir(&path) {
        Ok(rd) => {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue; // hide dotfiles/dirs by default
                }
                if e.path().is_dir() {
                    dirs.push(name);
                }
            }
            dirs.sort();
        }
        Err(e) => err = Some(e.to_string()),
    }
    let parent = path.parent().map(|p| p.to_string_lossy().to_string());
    Json(serde_json::json!({
        "path": path.to_string_lossy(),
        "parent": parent,
        "dirs": dirs,
        "error": err,
    }))
}


async fn get_events(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match st.sessions.events(&id).await {
        Ok(events) => (StatusCode::OK, Json(events)).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

async fn post_message(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PostMessage>,
) -> impl IntoResponse {
    match st
        .sessions
        .append_user_message(&id, &body.content, body.queue.as_deref())
        .await
    {
        Ok(line) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "line": line })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn post_start(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match st.loops.start(&id).await {
        Ok(pid) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "pid": pid })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn post_stop(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match st.loops.stop(&id).await {
        Ok(true) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Ok(false) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": false, "error": "no running loop" })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn post_approval(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PostApproval>,
) -> impl IntoResponse {
    match st.sessions.append_approval(&id, &body.id, &body.decision).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn post_rewind(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PostRewind>,
) -> impl IntoResponse {
    match st.sessions.append_rewind(&id, body.target_seq, &body.mode).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "rushi-web" }))
}

#[derive(Deserialize)]
struct PostRename {
    name: String,
}

/// Rename a session (stops any running loop first so the directory
/// move cannot strand a live process's paths).
async fn post_rename(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PostRename>,
) -> impl IntoResponse {
    if let Err(e) = st.loops.stop(&id).await {
        eprintln!("rushi-web: stop loop for '{}' failed: {e}", id);
    }
    match st.sessions.rename(&id, &body.name).await {
        Ok(()) => {
            (StatusCode::OK, Json(serde_json::json!({ "ok": true, "name": body.name })))
                .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Delete a session (stops any running loop first).
async fn delete_session(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = st.loops.stop(&id).await {
        eprintln!("rushi-web: stop loop for '{}' failed: {e}", id);
    }
    match st.sessions.delete(&id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

#[derive(Serialize)]
struct LoopState {
    running: bool,
}

/// Whether a loop is alive for `id`: one this server started, or a
/// stale `loop.pid` left by a previous server instance or the TUI.
async fn get_loop(State(st): State<AppState>, Path(id): Path<String>) -> Json<LoopState> {
    let mut running = st.loops.is_running(&id).await;
    if !running {
        let pf = st.sessions.session_dir(&id).join("loop.pid");
        if let Ok(txt) = std::fs::read_to_string(&pf) {
            if let Ok(pid) = txt.trim().parse::<u32>() {
                running = process::is_pid_alive(pid);
            }
        }
    }
    Json(LoopState { running })
}

// ── Goal panel (web-native equivalent of the TUI goal-ext) ─────────

async fn get_goal(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let dir = st.sessions.session_dir(&id);
    let view = goal::read(&dir);
    (StatusCode::OK, Json(view)).into_response()
}

#[derive(Deserialize)]
struct GoalBody {
    action: String,
    #[serde(default)]
    goal: Option<String>,
}

async fn post_goal(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GoalBody>,
) -> impl IntoResponse {
    // Ensure the session dir exists (a goal implies a session).
    if let Err(e) = tokio::fs::create_dir_all(st.sessions.session_dir(&id)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let action = goal::GoalAction {
        action: body.action,
        goal: body.goal,
    };
    match goal::apply_action(&st.sessions.session_dir(&id), &action) {
        Ok(g) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "goal": g }))).into_response(),
        Err(e) if e == "cleared" => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── WebSocket: live event stream + in-band commands ────────────────

async fn ws_handler(
    State(st): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, StatusCode> {
    Ok(ws.on_upgrade(move |socket| ws_session(socket, st, id)))
}

async fn ws_session(socket: WebSocket, st: AppState, session: String) {
    let (mut sock_tx, mut sock_rx) = socket.split();

    // 1. Send the full history on connect.
    if let Ok(events) = st.sessions.events(&session).await {
        let payload =
            format!("[{{\"kind\":\"history\",\"events\":{}}}]
", serde_json::to_string(&events).unwrap_or_default());
        if sock_tx.send(Message::text(payload)).await.is_err() {
            return;
        }
    }

    // 1b. Sidebar lamp snapshot: which sessions have a live loop
    // right now, so a fresh browser can light the breathing lamps of
    // sessions it never saw start (v0.5.13).
    {
        let running = st.loops.running_sessions().await;
        let payload = format!(
            "[{{\"kind\":\"loops\",\"data\":{}}}]",
            serde_json::to_string(&running).unwrap_or_else(|_| "[]".into())
        );
        if sock_tx.send(Message::text(payload)).await.is_err() {
            return;
        }
    }

    // 2. Watcher task: tail events.jsonl, push new lines over a channel.
    let path = st.sessions.events_path(&session);
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(256);
    let watcher = tokio::spawn(async move {
        sessions::tail_file(path, line_tx).await;
    });

    // 2b. Model stream channel: tail the session's live `.model-stream`
    // (the model subprocess writes one SSE delta JSON line per token)
    // so the client can render the in-progress assistant card live.
    let stream_path = st.sessions.session_dir(&session).join(".model-stream");
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<String>(256);
    let stream_watcher = tokio::spawn(async move {
        sessions::tail_model_stream(stream_path, stream_tx).await;
    });

    // Loop-lifecycle channel: the server tells this client when the
    // loop process for the session starts or dies.
    let mut loop_rx = st.loops.subscribe();

    // 3. Inbound commands from the client:
    //    {"kind":"message","content":"...","queue":"steer|follow"}
    //    {"kind":"approval","id":"...","decision":"approve|deny"}
    //    {"kind":"rewind","target_seq":N,"mode":"before|on"}
    //    {"kind":"start"}  /  {"kind":"stop"}
    let mut socket_dead = false;
    while !socket_dead {
        tokio::select! {
            line = line_rx.recv() => match line {
                Some(l) => {
                    if sock_tx.send(Message::text(format!("[{{\"kind\":\"event\",\"data\":{}}}]
", l))).await.is_err() {
                        break;
                    }
                }
                None => break, // watcher finished (connection to file lost)
            },
            line = stream_rx.recv() => match line {
                Some(l) => {
                    // Live model delta (the `.model-stream` side channel).
                    if sock_tx
                        .send(Message::text(format!("[{{\"kind\":\"model_stream\",\"data\":{}}}]
", l)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                None => break, // stream watcher finished
            },
            ev = loop_rx.recv() => match ev {
                Ok(e) => {
                    // v0.5.13: forward loop events for EVERY session,
                    // tagged with the session name — each client's
                    // sidebar needs the loop state of sessions it is
                    // not viewing (breathing lamps, green done bars).
                    // Clients scope the per-socket global flag and the
                    // error card to their own session by name.
                    let mut obj = serde_json::json!({
                        "kind": "loop_status",
                        "session": e.session,
                        "running": e.running,
                        "exit": e.exit,
                        "stopped": e.stopped,
                    });
                    if let Some(d) = &e.detail {
                        obj["detail"] = serde_json::Value::String(d.clone());
                    }
                    let frame = serde_json::json!([obj]);
                    if sock_tx.send(Message::text(frame.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(%n, "loop_status frames lagged for {session}");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
            },
            msg = sock_rx.next() => match msg {
                Some(Ok(Message::Text(b))) => {
                    let text = b.to_string();
                    let items: Vec<serde_json::Value> =
                        serde_json::from_str(&text).unwrap_or_default();
                    for item in items {
                        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                        match kind {
                            "message" => {
                                let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
                                let queue = item.get("queue").and_then(|v| v.as_str());
                                let _ = st
                                    .sessions
                                    .append_user_message(&session, content, queue)
                                    .await;
                            }
                            "approval" => {
                                let aid = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                let decision = item.get("decision").and_then(|v| v.as_str()).unwrap_or("deny");
                                let _ = st.sessions.append_approval(&session, aid, decision).await;
                            }
                            "rewind" => {
                                let target = item.get("target_seq").and_then(|v| v.as_u64()).unwrap_or(0);
                                let mode = item.get("mode").and_then(|v| v.as_str()).unwrap_or("before");
                                let _ = st.sessions.append_rewind(&session, target, mode).await;
                            }
                            "start" => {
                                match st.loops.start(&session).await {
                                    Ok(_) => {} // loop_status(true) went out via the broadcast
                                    Err(e) => {
                                        let msg = e.to_string();
                                        if msg.contains("already running") {
                                            // A loop is alive (tracked here or a stale pid) —
                                            // confirm running instead of alarming the client.
                                            let frame = serde_json::json!([
                                                { "kind": "loop_status", "session": session, "running": true }
                                            ]);
                                            let _ = sock_tx.send(Message::text(frame.to_string())).await;
                                        } else {
                                            let err = format!("[{{\"kind\":\"error\",\"message\":\"start failed: {msg}\"}}]
");
                                            let _ = sock_tx.send(Message::text(err)).await;
                                        }
                                    }
                                }
                            }
                            "stop" => {
                                let _ = st.loops.stop(&session).await;
                            }
                            _ => {}
                        }
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    let _ = sock_tx.send(Message::Pong(p)).await;
                }
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }

    watcher.abort();
    stream_watcher.abort();
}

// ── Static frontend ────────────────────────────────────────────────

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        "css.map" => "application/json",
        _ => "application/octet-stream",
    }
}

async fn serve_frontend(req: axum::extract::Request) -> Response {
    let rel = req.uri().path().trim_start_matches('/').to_string();
    let key = if rel.is_empty() { "index.html" } else { rel.as_str() };
    // Try the requested path first, then fall back to index.html for SPA
    // client-side routing.
    let file = assets::Frontend::get(key).or_else(|| assets::Frontend::get("index.html"));
    match file {
        Some(file) => {
            let is_html = key == "index.html" || key.ends_with(".html");
            Response::builder()
                .header(header::CONTENT_TYPE, if is_html { "text/html; charset=utf-8" } else { content_type(key) })
                .header(
                    header::CACHE_CONTROL,
                    if is_html { "no-cache" } else { "public, max-age=3600" },
                )
                .body(axum::body::Body::from(file.data.to_vec()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(axum::body::Body::from(
                "frontend not built: run `npm run build` in web/ and rebuild",
            ))
            .unwrap_or_default(),
    }
}

// ── main ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let mut host: &str = "127.0.0.1";
    let mut port: u16 = 8480;
    let mut sessions_root: Option<PathBuf> = None;
    let mut loop_cmd: Vec<String> = vec!["rushi".into(), "run".into()];
    let mut ext_dirs: Vec<PathBuf> = Vec::new();
    let mut config_path: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                if let Some(h) = args.get(i) {
                    host = h.as_str();
                }
            }
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(8480);
            }
            "--sessions-root" => {
                i += 1;
                sessions_root = args.get(i).map(PathBuf::from);
            }
            "--loop-cmd" => {
                i += 1;
                if let Some(cmd) = args.get(i) {
                    loop_cmd = cmd.split_whitespace().map(String::from).collect();
                }
            }
            "--ext-dir" => {
                i += 1;
                if let Some(d) = args.get(i) {
                    ext_dirs.push(PathBuf::from(d));
                }
            }
            "--config" => {
                i += 1;
                config_path = args.get(i).map(PathBuf::from);
            }
            "--help" | "-h" => {
                println!(
                    "rushi-web: WebUI front-end for the rushi harness\n\
                     \nUSAGE\n  rushi-web [OPTIONS]\n\
                     \nOPTIONS\n  --host <HOST>          bind address (default 127.0.0.1)\n\
                     \t  --port <PORT>          bind port (default 8480)\n\
                     \t  --sessions-root <DIR>  session directory\n\
                     \t  --loop-cmd <CMD...>    loop command (default: rushi run)\n\
                     \t  --ext-dir <DIR>        UI-extension directory (repeatable)\n\
                     \t  --config <FILE>        kernel config.toml to pin for spawned loops\n\
                     \t                       (exported to them as $CONFIG; any session working\n\
                     \t                       directory then stays a valid working directory)"
                );
                return Ok(());
            }
            _ => {
                i += 1;
                continue;
            }
        }
        i += 1;
    }

    let cfg = Arc::new(WebConfig {
        host: host.to_string(),
        port,
        sessions_root: sessions_root.unwrap_or_else(|| PathBuf::from("sessions")),
        loop_cmd,
        ext_dirs,
        config_path,
    });

    let sessions = Arc::new(SessionManager::new(cfg.clone()));
    let loops = Arc::new(LoopManager::new(cfg.clone()));
    let state = AppState {
        cfg,
        sessions,
        loops,
    };

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/browse", get(browse_dirs))
        .route("/api/default-cwd", get(get_default_cwd))
        .route("/api/sessions/{id}/events", get(get_events))
        .route("/api/sessions/{id}/messages", post(post_message))
        .route("/api/sessions/{id}/start", post(post_start))
        .route("/api/sessions/{id}/stop", post(post_stop))
        .route("/api/sessions/{id}/approval", post(post_approval))
        .route("/api/sessions/{id}/rewind", post(post_rewind))
        .route("/api/sessions/{id}/goal", get(get_goal).post(post_goal))
        .route("/api/sessions/{id}/rename", post(post_rename))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/sessions/{id}/loop", get(get_loop))
        .route("/ws/sessions/{id}", get(ws_handler))
        .fallback(serve_frontend)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    tracing::info!(%addr, "rushi-web listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
