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
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;

mod config;
mod essence;
mod files;
mod goal;
mod modelcfg;
mod process;
mod rewind;
mod sessions;
mod term;
mod time_inject;

use config::WebConfig;
use files::{list_files, raw_file, read_file};
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
    /// M15: the session-scoped pty registry (terminal tabs are
    /// session-bound — a shell outlives the WS connection that opened
    /// it and is re-attached, with ring replay, when the client comes
    /// back).
    terms: Arc<term::TermRegistry>,
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
    /// v0.5.45: the model entry this session should run (absent = follow
    /// the config's active entry).
    #[serde(default)]
    model: Option<String>,
    /// v0.5.45: this session's reasoning effort (absent = the entry's).
    #[serde(default)]
    effort: Option<String>,
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
        Ok(()) => {
            // v0.5.45: the new-session form also picks the model and the
            // thinking effort, recorded as the same markers the card chip
            // writes (they are applied in the session config at launch).
            if let Err(e) = st.sessions.set_model(name, body.model.as_deref()).await {
                return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
            }
            if let Err(e) = st.sessions.set_effort(name, body.effort.as_deref()).await {
                return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
            }
            (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
        }
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

/// Read-only history-tree projection (the rewind plugin's data source):
/// rounds = one user message each, forks at every rewind marker, the active
/// path + the "you are here" round. Never writes.
async fn get_rewind_tree(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match st.sessions.events(&id).await {
        Ok(events) => {
            // v0.5.69 (round-3 defect 1): while this session's loop is
            // alive, a tool call whose result is not written yet is
            // pending, not stranded — otherwise the projection would
            // drop every rewind marker for as long as the agent works.
            let live = st.loops.is_running(&id).await;
            (StatusCode::OK, Json(rewind::build_live(&id, &events, live))).into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

/// B2 (plan section 10.3): one round in full, for the flow view's top
/// panel — the *verbatim* user message (the tree carries a 60-char
/// preview). 404 when that line is not a round's `user_message`.
async fn get_rewind_node(
    State(st): State<AppState>,
    Path((id, seq)): Path<(String, u64)>,
) -> impl IntoResponse {
    match st.sessions.events(&id).await {
        Ok(events) => match rewind::node_detail(&events, seq) {
            Some(d) => (StatusCode::OK, Json(d)).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                format!("no round starts at line {seq}"),
            )
                .into_response(),
        },
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

/// Write a `rewind` marker. The R1 pre-check
/// (`docs/rewind-plugin-plan.md` 11.1) rides back in the response: the
/// kernel never refuses a marker, so the plugin is told what the
/// projection will do with it — `Ok` (it takes effect) or
/// `StrandsPair` (the kernel's P4 guard drops it, the user's rewind
/// does not happen; the client shows the notice, D-C). The marker is
/// written either way: the log is append-only and the client decides
/// whether to offer the pick.
async fn post_rewind(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PostRewind>,
) -> impl IntoResponse {
    let verdict = match st.sessions.events(&id).await {
        Ok(events) => {
            let live = st.loops.is_running(&id).await;
            rewind::rewind_verdict_live(&events, body.target_seq, &body.mode, live)
        }
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };
    // A pick the kernel would ignore is refused here, so the log never
    // gains a marker that does nothing (the pre-check is over the same
    // rule the projection applies). The verdict rides in the body
    // either way; the client shows the notice instead of a bare error.
    if !matches!(verdict, rewind::RewindVerdict::Ok) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "verdict": verdict })),
        )
            .into_response();
    }
    match st.sessions.append_rewind(&id, body.target_seq, &body.mode).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "verdict": verdict })),
        )
            .into_response(),
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
    // M15: a session's terminals are bound to it — killing the session
    // kills its shells (their cwd is about to disappear anyway).
    st.terms.purge(&id);
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

/// v0.5.55 P0: the running-loop set across ALL sessions, for resyncing
/// the sidebar's breathing lamps without a live WS connection. The client
/// calls this on load and on a timer; it is the server-side truth
/// (in-memory map + `loop.pid` liveness), so it is correct even after a
/// server restart, when the in-memory map is empty but orphaned loop
/// processes are still alive.
///
/// v0.5.55 P1: also returns `finished` — sessions with a `loop.last`
/// marker and no live loop, i.e. "loop ended, result not yet viewed".
async fn get_loops(State(st): State<AppState>) -> Json<process::LoopsSnapshot> {
    Json(st.loops.loops_snapshot().await)
}

/// v0.5.55 P1: the user viewed this session, so its "finished, unviewed"
/// green lamp is consumed. Deleting the `loop.last` marker stops the next
/// poll / reconnect from re-lighting the lamp. A *stale* `loop.pid`
/// (dead pid) is cleared too — otherwise the orphan sweep in
/// `loops_snapshot()` would re-create the marker for this same,
/// already-viewed exit and the lamp would come back. A *live* `loop.pid`
/// (a loop running right now) is left alone; its real exit will produce a
/// fresh marker the user should still be told about.
async fn post_loop_viewed(State(st): State<AppState>, Path(id): Path<String>) -> Json<serde_json::Value> {
    let dir = st.sessions.session_dir(&id);
    let _ = std::fs::remove_file(dir.join("loop.last"));
    if let Ok(t) = std::fs::read_to_string(dir.join("loop.pid")) {
        if let Ok(pid) = t.trim().parse::<u32>() {
            if !process::is_pid_alive(pid) {
                let _ = std::fs::remove_file(dir.join("loop.pid"));
            }
        }
    }
    Json(serde_json::json!({ "ok": true }))
}

// ── Goal panel (web-native equivalent of the TUI goal-ext) ─────────

async fn get_goal(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let dir = st.sessions.session_dir(&id);
    let view = goal::read(&dir);
    (StatusCode::OK, Json(view)).into_response()
}

/// Read a session's essence entries for the sidebar essence plugin. The
/// store is owned by the harness's `essence` tool; the server only reads
/// it, so this is read-only and always 200 (an empty list if absent).
async fn get_essence(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let view = essence::read(&st.sessions.session_dir(&id));
    (StatusCode::OK, Json(view)).into_response()
}

// ── context meter (v0.5.75) ────────────────────────────────────────
/// The numbers the context-meter panel cannot derive from the event log:
/// the system prompt's character count (the panel prices it with the
/// `ceil(chars / 4) + 4` heuristic) and the `[limits]` pair that owns the
/// context budget and the compaction reserve.
///
/// The panel anchors its whole composition to the provider-reported
/// prompt size, so this copy only has to be *close*: a stale system
/// prompt shifts the residual ("tools & injects") segment, it can never
/// make the panel contradict `usage`.
#[derive(Serialize)]
struct ContextMeta {
    system_chars: Option<u64>,
    budget: Option<u64>,
    compact_reserve: Option<u64>,
}

impl ContextMeta {
    fn unknown() -> Self {
        Self {
            system_chars: None,
            budget: None,
            compact_reserve: None,
        }
    }
}

/// Read the meta from the config file this session's LOOP actually runs:
/// the per-session derivative when one exists (`modelcfg` writes it on a
/// model edit, and the spawn hands that path to the loop), the shared
/// file otherwise — the same choice `process::spawn` makes.
fn read_context_meta(shared: &std::path::Path, session: &str) -> ContextMeta {
    let per_session = process::session_config_path(shared, session);
    let path = if per_session.exists() {
        per_session
    } else {
        shared.to_path_buf()
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ContextMeta::unknown();
    };
    let Ok(doc) = text.parse::<toml::Value>() else {
        return ContextMeta::unknown();
    };
    let limit = |key: &str| -> Option<u64> {
        doc.get("limits")
            .and_then(|l| l.get(key))
            .and_then(|v| v.as_integer())
            .and_then(|n| u64::try_from(n).ok())
    };
    ContextMeta {
        system_chars: doc
            .get("system_prompt")
            .and_then(|s| s.get("text"))
            .and_then(|v| v.as_str())
            .map(|t| t.chars().count() as u64),
        budget: limit("context_budget_tokens"),
        compact_reserve: limit("compact_reserve_tokens"),
    }
}

/// v0.5.75: the context meter's server-side numbers for one session.
async fn get_context_meta(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(shared) = st.cfg.config_path.as_deref() else {
        return (StatusCode::OK, Json(ContextMeta::unknown())).into_response();
    };
    (StatusCode::OK, Json(read_context_meta(shared, &id))).into_response()
}

/// v0.5.57: the session's latest compaction_summary event (the current
/// context-handoff summary). Served so the webui can pin a persistent
/// "context summary" card — the event itself lives deep in the event log
/// (often far outside the last-200 truncated window) and would otherwise
/// be invisible after a page reload.
async fn get_compaction(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match st.sessions.latest_compaction(&id).await {
        Some(ev) => (StatusCode::OK, Json(ev)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::Value::Null)).into_response(),
    }
}

// ── time-inject plugin (rushi-time-inject) ─────────────────────────

/// v0.5.56: the per-session time-inject toggle state. The hook
/// (harness-hook-time-inject) reads the `.time_inject` marker on every
/// model call, so toggling applies from the session's next model call —
/// no loop restart. Absent marker means ON (the hook's default).
async fn get_time_inject(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let enabled = st.sessions.time_inject_enabled(&id);
    (StatusCode::OK, Json(serde_json::json!({ "enabled": enabled }))).into_response()
}

#[derive(Deserialize)]
struct TimeInjectBody {
    enabled: bool,
}

/// v0.5.56: set the per-session time-inject toggle. `enabled: false`
/// writes the explicit off marker; `enabled: true` clears it (back to
/// the default-on state).
async fn post_time_inject(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TimeInjectBody>,
) -> impl IntoResponse {
    match st.sessions.set_time_inject(&id, body.enabled).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "enabled": body.enabled }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
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

// ── model settings ─────────────────────────────────────────────────

async fn get_model(State(st): State<AppState>) -> impl IntoResponse {
    match modelcfg::load(&st.cfg) {
        Ok(m) => (StatusCode::OK, Json(m)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn post_model(State(st): State<AppState>, Json(body): Json<modelcfg::ModelSettings>) -> impl IntoResponse {
    match modelcfg::save(&st.cfg, &body) {
        Ok(out) => (StatusCode::OK, Json(out)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn post_model_probe(
    State(st): State<AppState>,
    Json(body): Json<modelcfg::ProbeRequest>,
) -> impl IntoResponse {
    (StatusCode::OK, Json(modelcfg::probe(&st.cfg, &body).await)).into_response()
}

async fn post_model_models(
    State(st): State<AppState>,
    Json(body): Json<modelcfg::FetchRequest>,
) -> impl IntoResponse {
    (StatusCode::OK, Json(modelcfg::fetch_models(&st.cfg, &body).await)).into_response()
}

#[derive(Deserialize)]
struct ModelKeyBody {
    /// The env var name the entry declares in `api_key_env`.
    name: String,
    /// The key to store, or `null`/empty to forget it.
    #[serde(default)]
    value: Option<String>,
}

/// Store (or clear) a pasted provider key. The key never comes back out:
/// GET /api/model only reports whether one is available.
async fn post_model_key(
    State(st): State<AppState>,
    Json(body): Json<ModelKeyBody>,
) -> impl IntoResponse {
    match modelcfg::set_secret(&st.cfg, &body.name, body.value.as_deref()) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
struct SessionModelBody {
    /// The model entry this session should run, or `null` to follow the
    /// config's active entry again. v0.5.76: ABSENT leaves it untouched
    /// (mirrors `effort`) — the model popup's effort row posts only
    /// `effort` and must not disturb the model choice.
    #[serde(default, deserialize_with = "double_option")]
    model: Option<Option<String>>,
    /// v0.5.45: the session's reasoning effort, or `null` to keep the
    /// entry's own. Absent (rather than null) leaves it untouched — the
    /// card chip only edits the model.
    #[serde(default, deserialize_with = "double_option")]
    effort: Option<Option<String>>,
}

/// Tell "absent" from "explicit null" for an optional field.
fn double_option<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(de).map(Some)
}

async fn post_session_model(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SessionModelBody>,
) -> impl IntoResponse {
    if let Some(model) = body.model {
        if let Err(e) = st.sessions.set_model(&id, model.as_deref()).await {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    }
    if let Some(effort) = body.effort {
        if let Err(e) = st.sessions.set_effort(&id, effort.as_deref()).await {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "model": st.sessions.model_choice(&id),
            "effort": crate::sessions::read_marker(
                &st.sessions.session_dir(&id),
                crate::sessions::EFFORT_MARKER,
            ),
        })),
    )
        .into_response()
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

    // 1. Send the LAST HIST_PAGE events on connect (truncated history:
    // long sessions would otherwise ship their whole log — the 13 MB
    // case — as one frame). The client pages backwards with
    // "load_earlier" commands; line numbers are stable because
    // events.jsonl is append-only.
    const HIST_PAGE: u64 = 200;
    if let Ok(page) = st.sessions.events_windowed(&session, None, HIST_PAGE).await {
        let payload = format!(
            "[{{\"kind\":\"history\",\"events\":{},\"oldest_line\":{},\"total_lines\":{},\"has_more\":{},\"total_rounds\":{}}}]
",
            serde_json::to_string(&page.events).unwrap_or_default(),
            page.oldest_line,
            page.total_lines,
            page.has_more,
            page.total_rounds,
        );
        if sock_tx.send(Message::text(payload)).await.is_err() {
            return;
        }
    }

    // 1b. Sidebar lamp snapshot: which sessions have a live loop
    // right now, so a fresh browser can light the breathing lamps of
    // sessions it never saw start (v0.5.13). v0.5.55 P1: the snapshot
    // also carries the `finished` set (sessions with a `loop.last`
    // marker and no live loop) so the green "finished, unviewed" lamps
    // resync on reconnect too.
    {
        let snap = st.loops.loops_snapshot().await;
        let frame = serde_json::json!({
            "kind": "loops",
            "running": snap.running,
            "finished": snap.finished,
        });
        let payload = format!("[{frame}]");
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
    // v0.5.21: a fresh client has an empty live buffer, so replay the
    // whole in-flight stream file first (catch-up) — but only while
    // the loop that owns the file is alive; a stale file left by a
    // killed loop must not be replayed as if it were a live stream.
    let stream_path = st.sessions.session_dir(&session).join(".model-stream");
    let catch_up = st.loops.is_running(&session).await;
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<String>(256);
    let stream_watcher = tokio::spawn(async move {
        sessions::tail_model_stream(stream_path, stream_tx, catch_up).await;
    });

    // Loop-lifecycle channel: the server tells this client when the
    // loop process for the session starts or dies.
    let mut loop_rx = st.loops.subscribe();

    // M9/M11/M15: session terminals (PTY). The ptys live in the
    // session-scoped registry (`st.terms`): a session switch or a page
    // refresh disconnects THIS socket, but the shells keep running in
    // the registry, and reconnecting sockets re-attach. The registry's
    // reader threads fan each pty's chunks into this connection's
    // `term_tx`; the `term_rx` arm in the select! turns them into
    // frames. The channel carries (term_id, Some(bytes)) output chunks
    // and (term_id, None) EOF markers (the shell for that id exited).
    // `attach_token` is this connection's identity in the registry's
    // fanout lists — teardown detaches by token, not by kill.
    const TERM_CAP: u32 = 4;
    let attach_token = {
        static NEXT_ATTACH: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        NEXT_ATTACH.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    };
    let (term_tx, mut term_rx) =
        tokio::sync::mpsc::channel::<(u32, Option<Vec<u8>>)>((TERM_CAP as usize) * 64);

    // M15: re-attach to every pty this session still runs (a session
    // switch / refresh dropped the previous socket). Replay each slot's
    // ring tail so the client's terminals catch up, then report
    // liveness. (If the socket is already dead the select loop below
    // will break on its first failed send.)
    for (id, eof, replay) in st.terms.attach_all(&session, attach_token, &term_tx) {
        for chunk in &replay {
            let frame = serde_json::json!([{
                "kind": "term_out",
                "id": id,
                "data": chunk,
            }]);
            if sock_tx.send(Message::text(frame.to_string())).await.is_err() {
                break;
            }
        }
        let frame = serde_json::json!([{
            "kind": "term_status",
            "id": id,
            "running": !eof,
        }]);
        if sock_tx.send(Message::text(frame.to_string())).await.is_err() {
            break;
        }
    }

    // 3. Inbound commands from the client:
    //    {"kind":"message","content":"...","queue":"steer|follow"}
    //    {"kind":"approval","id":"...","decision":"approve|deny"}
    //    {"kind":"rewind","target_seq":N,"mode":"before|on"}
    //    {"kind":"start"}  /  {"kind":"stop"}
    let socket_dead = false;
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
                Ok(e) => {                    // v0.5.13: forward loop events for EVERY session,
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
            chunk = term_rx.recv() => {
                // M9/M11/M15: pty master output, tagged with the
                // terminal id, fanned out from the session registry's
                // reader threads through this connection's attach
                // channel. A (id, None) chunk is a reader's EOF
                // marker — the shell for that id exited naturally; the
                // registry keeps the (dead) slot so a restart can
                // reuse the id, so here we just report running:false
                // for the client's exit overlay.
                match chunk {
                    Some((id, Some(bytes))) => {
                        // A stray chunk for a pty that was already
                        // closed (in flight when `term_close` landed)
                        // is dropped.
                        if !st.terms.has(&session, id) {
                            continue;
                        }
                        let frame = serde_json::json!([{
                            "kind": "term_out",
                            "id": id,
                            "data": term::b64encode(&bytes),
                        }]);
                        if sock_tx.send(Message::text(frame.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some((id, None)) => {
                        let frame = serde_json::json!([{
                            "kind": "term_status",
                            "id": id,
                            "running": false,
                        }]);
                        if sock_tx.send(Message::text(frame.to_string())).await.is_err() {
                            break;
                        }
                    }
                    None => {} // the term channel closed; the other arms continue
                }
            }
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
                            // v0.5.17: page older events backwards.
                            // before_line = 1-based line number of the
                            // oldest event the client already holds
                            // (from the "history" / "history_page"
                            // frame's oldest_line). Line numbers are
                            // stable: events.jsonl is append-only.
                            "load_earlier" => {
                                let before = item.get("before_line").and_then(|v| v.as_u64()).unwrap_or(1);
                                let limit = item.get("limit").and_then(|v| v.as_u64()).unwrap_or(HIST_PAGE);
                                match st.sessions.events_windowed(&session, Some(before), limit).await {
                                    Ok(page) => {
                                        let frame = serde_json::json!([{
                                            "kind": "history_page",
                                            "events": page.events,
                                            "oldest_line": page.oldest_line,
                                            "total_lines": page.total_lines,
                                            "has_more": page.has_more,
                                            "total_rounds": page.total_rounds,
                                        }]);
                                        if sock_tx.send(Message::text(frame.to_string())).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        let err = format!(
                                            "[{{\"kind\":\"error\",\"message\":\"load_earlier failed: {}\"}}]\n",
                                            e
                                        );
                                        let _ = sock_tx.send(Message::text(err)).await;
                                    }
                                }
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
                            // ── M9/M11: terminal frames (multi-pty, id-tagged) ──
                            // ── M9/M11/M15: terminal frames (multi-pty, id-tagged) ──
                            "term_open" => {
                                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let cols = item.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u32;
                                let rows = item.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u32;
                                // M15 spawn-or-attach: a live pty with this
                                // id (left running by a session switch or a
                                // page refresh) is re-attached and its ring
                                // tail is replayed instead of spawning a
                                // second shell; a dead (EOF) slot or a
                                // missing one spawns a fresh pty under the
                                // same id ("restart shell" path).
                                match files::session_workdir(&st, &session) {
                                    Some(wd) => {
                                        let attach = Some(term_tx.clone());
                                        match st.terms.open(
                                            &session,
                                            id,
                                            cols,
                                            rows,
                                            &wd,
                                            attach_token,
                                            &attach,
                                        ) {
                                        Ok(replay) => {
                                            for chunk in &replay {
                                                let frame =
                                                    serde_json::json!([{
                                                        "kind": "term_out",
                                                        "id": id,
                                                        "data": chunk,
                                                    }]);
                                                if sock_tx.send(Message::text(frame.to_string())).await.is_err() {
                                                    break;
                                                }
                                            }
                                            let frame =
                                                serde_json::json!([{"kind":"term_status","id":id,"running":true}]);
                                            let _ = sock_tx.send(Message::text(frame.to_string())).await;
                                        }
                                        Err(e) => {
                                            let frame = serde_json::json!([{
                                                "kind": "term_status",
                                                "id": id,
                                                "running": false,
                                                "error": e,
                                            }]);
                                            let _ = sock_tx.send(Message::text(frame.to_string())).await;
                                        }
                                        }
                                    }
                                    None => {
                                        let frame = serde_json::json!([{
                                            "kind": "term_status",
                                            "id": id,
                                            "running": false,
                                            "error": "term_open: no working directory for this session",
                                        }]);
                                        let _ = sock_tx.send(Message::text(frame.to_string())).await;
                                    }
                                }
                            }
                            "term_input" => {
                                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let data = item.get("data").and_then(|v| v.as_str()).unwrap_or("");
                                if let Ok(bytes) = term::b64decode(data) {
                                    // M15: the pty lives in the session
                                    // registry, not on this socket — input
                                    // works even right after a reconnect,
                                    // before the attach replays.
                                    st.terms.input(&session, id, &bytes);
                                }
                            }
                            "term_resize" => {
                                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let cols = item.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u32;
                                let rows = item.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u32;
                                st.terms.resize(&session, id, cols, rows);
                            }
                            "term_close" => {
                                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                // M15: the ONE path that kills a pty on
                                // demand (explicit tab close). The session
                                // deletion REST purges the whole session;
                                // a bare disconnect does NOT.
                                st.terms.close(&session, id);
                                let frame = serde_json::json!([{"kind":"term_status","id":id,"running":false}]);
                                let _ = sock_tx.send(Message::text(frame.to_string())).await;
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

    // M15: disconnecting this socket does NOT kill the shells — they
    // belong to the session and keep running in the registry (their
    // output accumulates in each slot's ring). Detach this connection's
    // fanout entries; the ptys die on explicit `term_close`, on session
    // deletion, or when the server itself exits.
    st.terms.detach(&session, attach_token);

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

/// The static frontend as its own router, so the compression layer wraps
/// only these responses. Scoped on purpose: a layer on the whole router
/// would sit in front of the websocket upgrade and the streaming
/// endpoints as well.
fn frontend() -> Router {
    Router::new()
        .fallback(serve_frontend)
        .layer(CompressionLayer::new())
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
                    // Trunk writes the content hash into every asset file
                    // name, so a non-html asset can never go stale under
                    // its own URL: cache it for good. index.html keeps the
                    // hash of the current build and must be revalidated.
                    if is_html {
                        "no-cache"
                    } else {
                        "public, max-age=31536000, immutable"
                    },
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

// ── kernel config ───────────────────────────────────────────────────

/// The keys the webui consumes from the kernel `config.toml`.
struct KernelConfig {
    /// Absolute sessions root. A relative `[paths].sessions_root` is
    /// resolved against the config file's own directory — the kernel
    /// resolves it against the *loop's* cwd, and the webui spawns loops
    /// in each session's working directory, so the webui must hand the
    /// loop an absolute path or the two disagree about the session tree.
    sessions_root: Option<PathBuf>,
    host: Option<String>,
    port: Option<u16>,
}

/// Read the kernel config the webui shares with the loops it spawns.
/// Returns `None` when the file is missing or unreadable — the caller
/// falls back to CLI flags and defaults.
fn read_kernel_config(path: &std::path::Path) -> Option<KernelConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: toml::Value = text.parse().ok()?;
    let config_dir = path
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let sessions_root = doc
        .get("paths")
        .and_then(|p| p.get("sessions_root"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .map(|p| {
            if p.is_absolute() {
                p
            } else {
                config_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(p)
            }
        });

    let web = doc.get("web");
    let host = web
        .and_then(|w| w.get("host"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let port = web
        .and_then(|w| w.get("port"))
        .and_then(|v| v.as_integer())
        .and_then(|n| u16::try_from(n).ok());

    Some(KernelConfig {
        sessions_root,
        host,
        port,
    })
}

// ── main ────────────────────────────────────────────────────────────

/// Resolve the `--loop-cmd` words at `args[i]` (the flag itself).
///
/// The documented form is `--loop-cmd <CMD...>`, so every following word
/// belongs to the command — whether the shell handed them over as one
/// quoted element (`--loop-cmd "/k/rushi run"`) or as separate ones
/// (`--loop-cmd /k/rushi run`). Consuming a single argv element here is
/// what let a loop command lose its `run` and then die with
/// `unrecognized subcommand '<session>'` on every start; the stray word
/// fell into the catch-all arm and vanished.
///
/// Returns the words and how many argv elements were consumed (the flag
/// plus its words).
fn parse_loop_cmd(args: &[String], i: usize) -> (Vec<String>, usize) {
    let mut words: Vec<String> = Vec::new();
    let mut used = 1; // the flag itself
    let mut next = i + 1;
    while let Some(w) = args.get(next) {
        if w.starts_with("--") {
            break;
        }
        words.extend(w.split_whitespace().map(String::from));
        used += 1;
        next += 1;
    }
    (words, used)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    // CLI flags override the kernel config; whatever is left unset comes
    // from `[paths].sessions_root` / `[web].host` / `[web].port`.
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut sessions_root: Option<PathBuf> = None;
    let mut loop_cmd: Vec<String> = vec!["rushi".into(), "run".into()];
    let mut config_path: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                if let Some(h) = args.get(i) {
                    host = Some(h.clone());
                }
            }
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok());
            }
            "--sessions-root" => {
                i += 1;
                sessions_root = args.get(i).map(PathBuf::from);
            }
            "--loop-cmd" => {
                let (words, used) = parse_loop_cmd(&args, i);
                if !words.is_empty() {
                    loop_cmd = words;
                }
                i += used;
            }
            "--config" => {
                i += 1;
                config_path = args.get(i).map(PathBuf::from);
            }
            "--help" | "-h" => {
                println!(
                    "rushi-web: WebUI front-end for the rushi harness\n\
                     \nUSAGE\n  rushi-web [OPTIONS]\n\
                     \nOPTIONS\n  --config <FILE>        kernel config.toml (default keys below)\n\
                     \t  --host <HOST>          bind address (config [web].host, else 127.0.0.1)\n\
                     \t  --port <PORT>          bind port (config [web].port, else 8480)\n\
                     \t  --sessions-root <DIR>  session directory (config [paths].sessions_root)\n\
                     \t  --loop-cmd <CMD...>    loop command (default: rushi run)\n\
                     \nThe webui is self-wired: it reads the kernel config you pass with\n\
                     --config for [paths].sessions_root and [web].host/port, and pins\n\
                     that same file to every loop it spawns as $CONFIG. CLI flags win\n\
                     over the config file."
                );
                return Ok(());
            }
            other => {
                // Silently swallowing an argument is how a loop command
                // lost its `run`: `--loop-cmd /k/rushi run` (unquoted)
                // left `run` here, the command became just the binary,
                // and every start died with "unrecognized subcommand
                // '<session>'". Say so instead.
                eprintln!("rushi-web: warning: ignoring unrecognized argument {other:?}");
                i += 1;
                continue;
            }
        }
        i += 1;
    }

    // The webui's only build-time-free contract with the kernel: read the
    // same config.toml the loop reads (docs/itches.md, 2026-09-20 — the
    // kernel hosts no front-end launcher subcommand, so the front-end
    // self-wires).
    let kernel_cfg = config_path.as_deref().and_then(read_kernel_config);

    let cfg = Arc::new(WebConfig {
        host: host
            .or_else(|| kernel_cfg.as_ref().and_then(|k| k.host.clone()))
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        port: port
            .or_else(|| kernel_cfg.as_ref().and_then(|k| k.port))
            .unwrap_or(8480),
        sessions_root: sessions_root
            .or_else(|| kernel_cfg.as_ref().and_then(|k| k.sessions_root.clone()))
            .unwrap_or_else(|| PathBuf::from("sessions")),
        loop_cmd,
        config_path,
    });

    // Resolved before `cfg` moves into the router state below.
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;

    let sessions = Arc::new(SessionManager::new(cfg.clone()));
    let loops = Arc::new(LoopManager::new(cfg.clone()));
    let terms = Arc::new(term::TermRegistry::new());
    let state = AppState {
        cfg,
        sessions,
        loops,
        terms,
    };

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/browse", get(browse_dirs))
        // M8: workspace files for the right panel (cwd-escape guarded).
        .route("/api/files", get(list_files))
        .route("/api/file", get(read_file))
        .route("/api/raw", get(raw_file))
        .route("/api/default-cwd", get(get_default_cwd))
        .route("/api/sessions/{id}/events", get(get_events))
        .route("/api/sessions/{id}/messages", post(post_message))
        .route("/api/sessions/{id}/start", post(post_start))
        .route("/api/sessions/{id}/stop", post(post_stop))
        .route("/api/sessions/{id}/approval", post(post_approval))
        .route("/api/sessions/{id}/rewind", get(get_rewind_tree).post(post_rewind))
        .route("/api/sessions/{id}/rewind/node/{seq}", get(get_rewind_node))
        .route("/api/sessions/{id}/goal", get(get_goal).post(post_goal))
        .route("/api/sessions/{id}/essence", get(get_essence))
        .route("/api/sessions/{id}/context-meta", get(get_context_meta))
        .route("/api/sessions/{id}/compaction", get(get_compaction))
        .route(
            "/api/sessions/{id}/time-inject",
            get(get_time_inject).post(post_time_inject),
        )
        .route("/api/model", get(get_model).post(post_model))
        .route("/api/model/probe", post(post_model_probe))
        .route("/api/model/models", post(post_model_models))
        .route("/api/model/key", post(post_model_key))
        .route("/api/sessions/{id}/model", post(post_session_model))
        .route("/api/sessions/{id}/rename", post(post_rename))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/sessions/{id}/loop", get(get_loop))
        .route("/api/sessions/{id}/loop/viewed", post(post_loop_viewed))
        .route("/api/loops", get(get_loops))
        .route("/ws/sessions/{id}", get(ws_handler))
        .fallback_service(frontend())
        .layer(CorsLayer::permissive())
        .with_state(state);

    tracing::info!(%addr, "rushi-web listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_loop_cmd;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// v0.5.76: the model popup's effort row posts only `effort`. The
    /// absent `model` must therefore mean "leave the model alone"; an
    /// explicit `null` still clears it and a string still sets it.
    #[test]
    fn session_model_body_distinguishes_absent_from_null() {
        let b: super::SessionModelBody = serde_json::from_str(r#"{"effort":"high"}"#).unwrap();
        assert!(b.model.is_none(), "absent model = untouched");
        assert_eq!(b.effort, Some(Some("high".to_string())));

        let b: super::SessionModelBody = serde_json::from_str(r#"{"model":null}"#).unwrap();
        assert_eq!(b.model, Some(None), "explicit null = clear");
        assert!(b.effort.is_none(), "absent effort = untouched");

        let b: super::SessionModelBody =
            serde_json::from_str(r#"{"model":"m1","effort":null}"#).unwrap();
        assert_eq!(b.model, Some(Some("m1".to_string())));
        assert_eq!(b.effort, Some(None), "explicit null effort = clear");
    }

    /// `--loop-cmd "/k/rushi run"`: the shell handed over one element.
    #[test]
    fn loop_cmd_takes_one_quoted_element() {
        let a = args(&["rushi-web", "--loop-cmd", "/k/rushi run"]);
        let (words, used) = parse_loop_cmd(&a, 1);
        assert_eq!(words, vec!["/k/rushi", "run"]);
        assert_eq!(used, 2);
    }

    /// `--loop-cmd /k/rushi run`: the shell handed over two. Regression
    /// guard — taking only the next element is what dropped `run` and
    /// made every loop start die with
    /// `unrecognized subcommand '<session>'`.
    #[test]
    fn loop_cmd_takes_unquoted_words_up_to_the_next_flag() {
        let a = args(&["rushi-web", "--loop-cmd", "/k/rushi", "run", "--port", "8480"]);
        let (words, used) = parse_loop_cmd(&a, 1);
        assert_eq!(words, vec!["/k/rushi", "run"]);
        assert_eq!(used, 3);
        assert_eq!(&a[1 + used], "--port", "parsing resumes at the next flag");
    }

    #[test]
    fn loop_cmd_with_no_words_keeps_the_default() {
        let a = args(&["rushi-web", "--loop-cmd"]);
        let (words, used) = parse_loop_cmd(&a, 1);
        assert!(words.is_empty());
        assert_eq!(used, 1);
    }
}
