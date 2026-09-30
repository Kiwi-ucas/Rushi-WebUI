//! M8: workspace-file endpoints backing the right panel's Files tab:
//!
//! - `GET /api/files?session=&path=` — one directory's listing (the
//!   Files tree loads lazily, one request per expanded directory;
//!   `path` is workdir-relative, empty = the workdir root).
//! - `GET /api/file?session=&path=`  — text/code content for the
//!   preview pane (capped; binary files → 415 so the client falls
//!   back to "no text preview").
//! - `GET /api/raw?session=&path=`   — raw bytes for image previews.
//!
//! cwd-escape protection: every request is re-validated against the
//! session's resolved workdir root. The path must be relative; after
//! `canonicalize()` (which also resolves symlinks) it must still be
//! inside the canonical root, or the request is rejected with 403.

use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::AppState;

/// Cap for the text preview endpoint — matches the JS `MAX_PREVIEW`
/// (512 KiB); larger files serve the first chunk with `truncated:true`.
const FILE_PREVIEW_CAP: u64 = 512 * 1024;

/// Cap for the raw (image) endpoint — 15 MiB.
const RAW_CAP: u64 = 15 * 1024 * 1024;

#[derive(Deserialize)]
pub struct FilesQuery {
    pub session: String,
    /// workdir-relative directory or file; empty/absent = the root.
    #[serde(default)]
    pub path: Option<String>,
}

/// The session's working directory: the session's cwd marker (the
/// kernel's `cwd`, else the webui's `.cwd`) when that directory still
/// exists, else the default workdir (the sessions root's parent) — the
/// same resolution the loop runner uses (process.rs). Shared by the M8
/// file endpoints and the M9 terminal (`term_open` spawns the shell
/// here).
pub fn session_workdir(st: &AppState, session: &str) -> Option<PathBuf> {
    let marked = st
        .sessions
        .cwd_marker(session)
        .map(PathBuf::from)
        .filter(|p| p.is_dir());
    marked.or_else(|| {
        st.cfg
            .sessions_root
            .parent()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
    })
}

/// Resolve `rel` (workdir-relative) inside the session's workdir.
///
/// The workdir is the session's `.cwd` marker when that directory
/// still exists, else the default workdir (the sessions root's
/// parent) — the same resolution the loop runner uses (process.rs).
/// `rel` must be relative; the canonical result must stay inside the
/// canonical root (symlinks pointing out are rejected). Returns
/// (root, target) or a (status, message) rejection.
async fn resolve_in_workdir(
    st: &AppState,
    session: &str,
    rel: &str,
) -> Result<(PathBuf, PathBuf), (StatusCode, String)> {
    let root = match session_workdir(st, session) {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                "no working directory for this session".to_string(),
            ))
        }
    };
    let root = root
        .canonicalize()
        .map_err(|e| (StatusCode::NOT_FOUND, format!("workdir missing: {e}")))?;

    let rel = rel.trim().trim_matches('/');
    if Path::new(rel).is_absolute() {
        return Err((
            StatusCode::FORBIDDEN,
            "absolute paths are not allowed".to_string(),
        ));
    }
    if rel.is_empty() {
        return Ok((root.clone(), root));
    }
    let target = root.join(rel);
    let target = match target.canonicalize() {
        Ok(t) => t,
        Err(e) => {
            return Err((StatusCode::NOT_FOUND, format!("no such path: {e}")));
        }
    };
    if !target.starts_with(&root) {
        return Err((
            StatusCode::FORBIDDEN,
            "path escapes the session's working directory".to_string(),
        ));
    }
    Ok((root, target))
}

fn reject(status: StatusCode, msg: &str) -> Response {
    (status, msg.to_string()).into_response()
}

/// Directory listing for the Files tree: dirs first, then files,
/// case-insensitive by name. Dotfiles are included (the panel is a
/// developer tool; `.git` etc. are worth seeing). Synchronous std::fs
/// read_dir, same idiom as browse_dirs in main.rs.
pub async fn list_files(
    State(st): State<AppState>,
    Query(q): Query<FilesQuery>,
) -> Response {
    let rel = q.path.as_deref().unwrap_or("");
    let (root, target) = match resolve_in_workdir(&st, &q.session, rel).await {
        Ok(v) => v,
        Err((code, msg)) => return reject(code, &msg),
    };
    let mut entries: Vec<(String, bool, Option<u64>)> = Vec::new();
    let rd = match std::fs::read_dir(&target) {
        Ok(rd) => rd,
        Err(e) => {
            return reject(StatusCode::NOT_FOUND, &format!("cannot list: {e}"));
        }
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.path().is_dir();
        let size = if is_dir {
            None
        } else {
            e.metadata().ok().map(|m| m.len())
        };
        entries.push((name, is_dir, size));
    }
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            a.0.to_lowercase().cmp(&b.0.to_lowercase())
        })
    });
    Json(json!({
        "cwd": root.to_string_lossy(),
        "path": rel,
        "entries": entries.iter().map(|(n, d, s)| json!({
            "name": n,
            "is_dir": d,
            "size": s,
        })).collect::<Vec<Value>>(),
    }))
    .into_response()
}

/// Text/code content for the preview pane. Binary files (a NUL byte in
/// the served chunk) → 415; oversized files serve the first
/// `FILE_PREVIEW_CAP` bytes with `truncated: true`.
pub async fn read_file(
    State(st): State<AppState>,
    Query(q): Query<FilesQuery>,
) -> Response {
    let rel = q.path.as_deref().unwrap_or("");
    let (_root, target) = match resolve_in_workdir(&st, &q.session, rel).await {
        Ok(v) => v,
        Err((code, msg)) => return reject(code, &msg),
    };
    let meta = match fs::metadata(&target).await {
        Ok(m) => m,
        Err(e) => return reject(StatusCode::NOT_FOUND, &e.to_string()),
    };
    if meta.is_dir() {
        return reject(StatusCode::BAD_REQUEST, "path is a directory");
    }
    let size = meta.len();
    let cap = size.min(FILE_PREVIEW_CAP);
    let mut buf = vec![0u8; cap as usize];
    let mut file = match fs::File::open(&target).await {
        Ok(f) => f,
        Err(e) => return reject(StatusCode::NOT_FOUND, &e.to_string()),
    };
    if let Err(e) = file.read_exact(&mut buf).await {
        return reject(StatusCode::NOT_FOUND, &e.to_string());
    }
    if buf.iter().any(|b| *b == 0) {
        return reject(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "binary file: no text preview",
        );
    }
    Json(json!({
        "path": rel,
        "content": String::from_utf8_lossy(&buf),
        "size": size,
        "truncated": size > FILE_PREVIEW_CAP,
    }))
    .into_response()
}

/// Raw bytes for image previews. Same escape guard; capped at
/// `RAW_CAP` (413).
pub async fn raw_file(
    State(st): State<AppState>,
    Query(q): Query<FilesQuery>,
) -> Response {
    let rel = q.path.as_deref().unwrap_or("");
    let (_root, target) = match resolve_in_workdir(&st, &q.session, rel).await {
        Ok(v) => v,
        Err((code, msg)) => return reject(code, &msg),
    };
    let meta = match fs::metadata(&target).await {
        Ok(m) => m,
        Err(_) => return reject(StatusCode::NOT_FOUND, "no such file"),
    };
    if meta.is_dir() {
        return reject(StatusCode::BAD_REQUEST, "path is a directory");
    }
    if meta.len() > RAW_CAP {
        return reject(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file too large for preview",
        );
    }
    let bytes = match fs::read(&target).await {
        Ok(b) => b,
        Err(e) => return reject(StatusCode::NOT_FOUND, &e.to_string()),
    };
    let mime = mime_for_ext(&target);
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn mime_for_ext(p: &Path) -> &'static str {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}
