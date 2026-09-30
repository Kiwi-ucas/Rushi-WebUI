//! Model settings: read and write the model section of the kernel config.
//!
//! The settings panel edits three things in the kernel `config.toml`:
//! `[model]` (the global defaults the kernel honours), one
//! `[model."<name>"]` table per provider entry, and `[active] model`.
//!
//! That file is hand-maintained, comment-heavy, and untracked by git, so
//! the write path is deliberately conservative:
//!
//! - `toml_edit` changes only the keys this module owns and leaves every
//!   other byte alone (comments, formatting, `[hooks]`, `[system_prompt]`,
//!   `[web]`). Never round-trip through `toml::to_string`.
//! - Values are validated against the kernel's own reader
//!   (`rushi/crates/rushi/src/model_settings.rs`), because the kernel is
//!   silent about the mistakes that matter: a wrong type falls back to the
//!   default, an unknown key is ignored, and an `[active]` naming a
//!   missing entry drops every setting to the built-in defaults.
//! - The previous file is backed up, the new one is written through a
//!   temp file + rename (loop stage binaries re-read the config on every
//!   spawn, so a half-written file must never be observable), and the
//!   `kernelsync/config.local.toml` mirror is updated too — `sync.sh`
//!   restores the kernel copy from that mirror, so a model change that
//!   skips it would be silently rolled back on the next sync.
//!
//! The wire types here mirror `web-leptos/src/model.rs` (the editor is a
//! WASM build with no toml dependency). Change both sides together.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
// `toml` is the read-only parser (load); `toml_edit` is the writer (save).
use toml_edit::{DocumentMut, Item, Table};

use crate::config::WebConfig;

/// The `[model]` keys the kernel falls back to for every entry. Anything
/// else written at the global level is ignored, so the panel only offers
/// these.
pub const GLOBAL_KEYS: [&str; 5] = [
    "max_output_tokens",
    "reasoning_effort",
    "model_timeout_s",
    "estimate_chars_per_token",
    "vision",
];

/// `reasoning_effort` values the kernel maps to thinking levels. It does
/// not validate them (a typo is sent to the provider verbatim), so this is
/// a warning list, not a gate.
pub const EFFORT_VALUES: [&str; 7] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// Legacy `[paths]`/`[hooks]` keys the v0.1.5 kernel refuses to load
/// (`crates/rushi/src/config_check.rs`). The panel must never introduce
/// them; this is a belt-and-braces check on the rendered result.
const LEGACY_KEYS: [(&str, &str); 3] = [
    ("paths", "tools_root"),
    ("paths", "extra_tools_roots"),
    ("hooks", "on"),
];

// ── wire types (mirrored in web-leptos/src/model.rs) ────────────────

/// One `[model."<name>"]` entry. Every field is optional: `None` (or an
/// empty string from the form) means "key absent — use the kernel's
/// default", and saving removes the key.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub vision: Option<bool>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub estimate_chars_per_token: Option<u64>,
    /// Read-only: keys the file carries that the panel does not edit
    /// (e.g. `api`, which the kernel never reads). Shown so the user can
    /// see them, preserved untouched on save.
    #[serde(default)]
    pub extra_keys: Vec<String>,
}

/// The global `[model]` defaults.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Globals {
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub model_timeout_s: Option<u64>,
    #[serde(default)]
    pub estimate_chars_per_token: Option<u64>,
    #[serde(default)]
    pub vision: Option<bool>,
}

/// What the panel shows and edits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSettings {
    pub active: String,
    pub entries: Vec<ModelEntry>,
    pub globals: Globals,
    /// The kernel config being edited.
    pub config_path: String,
    /// The `kernelsync/config.local.toml` mirror, when one was found.
    #[serde(default)]
    pub mirror_path: Option<String>,
    /// For every `api_key_env` in use: is that variable present in the
    /// webui server's environment? The key itself never reaches the
    /// browser — only this yes/no.
    pub key_env_present: BTreeMap<String, bool>,
    /// `model --describe` output: the values actually in effect, resolved
    /// by the kernel's own code rather than re-derived here.
    #[serde(default)]
    pub effective: Option<serde_json::Value>,
    /// Read-only notes about the file (dead keys, oddities).
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Result of a save: what changed and what needs a loop restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SaveOutcome {
    /// Human-readable reasons the running loops must be restarted for the
    /// change to take full effect (the loop snapshots part of the config
    /// at startup; the stage binaries re-read it every spawn).
    pub needs_loop_restart: Vec<String>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub backup: Option<String>,
    #[serde(default)]
    pub mirror: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProbeRequest {
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeResult {
    pub ok: bool,
    /// The endpoint that answered (or the last one tried).
    pub endpoint: String,
    pub status: Option<u16>,
    pub detail: String,
}

// ── paths ───────────────────────────────────────────────────────────

fn config_path(cfg: &WebConfig) -> Result<PathBuf, String> {
    cfg.config_path
        .clone()
        .ok_or_else(|| "webui 启动时没有 --config，无法定位内核 config.toml".to_string())
}

/// The `kernelsync/config.local.toml` mirror that `sync.sh` restores the
/// kernel config from. `[web].config_mirror` (relative to the config dir)
/// pins it; otherwise the sibling-checkout layout is assumed and only
/// used when the file actually exists.
fn mirror_path(config: &Path) -> Option<PathBuf> {
    let dir = config.parent()?;
    let text = std::fs::read_to_string(config).ok()?;
    let doc: toml::Value = text.parse().ok()?;
    let explicit = doc
        .get("web")
        .and_then(|w| w.get("config_mirror"))
        .and_then(|v| v.as_str())
        .map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                dir.join(p)
            }
        });
    match explicit {
        Some(p) if p.exists() => Some(p),
        Some(_) => None,
        None => {
            let guess = dir.join("../rushi-webui/kernelsync/config.local.toml");
            guess.exists().then(|| guess)
        }
    }
}

// ── load ────────────────────────────────────────────────────────────

pub fn load(cfg: &WebConfig) -> Result<ModelSettings, String> {
    let path = config_path(cfg)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    let doc: toml::Value = text
        .parse()
        .map_err(|e| format!("config.toml 解析失败: {e}"))?;

    let model = doc.get("model").and_then(|m| m.as_table());
    let entries: Vec<ModelEntry> = model
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.is_table())
                .map(|(name, v)| entry_from_value(name, v))
                .collect()
        })
        .unwrap_or_default();

    let globals = Globals {
        max_output_tokens: global_int(model, "max_output_tokens"),
        reasoning_effort: global_str(model, "reasoning_effort"),
        model_timeout_s: global_int(model, "model_timeout_s"),
        estimate_chars_per_token: global_int(model, "estimate_chars_per_token"),
        vision: model
            .and_then(|m| m.get("vision"))
            .and_then(|v| v.as_bool()),
    };

    let active = doc
        .get("active")
        .and_then(|a| a.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let mut key_env_present = BTreeMap::new();
    for e in &entries {
        let name = e
            .api_key_env
            .clone()
            .unwrap_or_else(|| "MODEL_API_KEY".to_string());
        key_env_present.insert(name.clone(), std::env::var(&name).is_ok());
    }

    let mut notes = Vec::new();
    if let Some(m) = model {
        if m.contains_key("api") {
            notes.push(
                "`[model] api` 是本机配置里的**死键**：内核从不读它，`bin/model` 永远先发 \
                 /v1/responses，只有 404/405 才回退 /v1/chat/completions。面板保留该键不动。"
                    .to_string(),
            );
        }
    }
    if !active.is_empty() && !entries.iter().any(|e| e.name == active) {
        notes.push(format!(
            "`[active] model = \"{active}\"` 在 [model] 里找不到对应条目 —— 内核会**静默**退回\
             全部默认值（base_url 127.0.0.1:8080 等）。"
        ));
    }

    let mirror = mirror_path(&path).map(|p| p.canonicalize().unwrap_or(p));
    let effective = describe(&path);

    Ok(ModelSettings {
        active,
        entries,
        globals,
        config_path: path.display().to_string(),
        mirror_path: mirror.map(|p| p.display().to_string()),
        key_env_present,
        effective,
        notes,
    })
}

fn entry_from_value(name: &str, v: &toml::Value) -> ModelEntry {
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|x| !x.is_empty())
    };
    let n = |k: &str| v.get(k).and_then(|x| x.as_integer()).and_then(|x| u64::try_from(x).ok());
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool());

    let known = [
        "model_id",
        "base_url",
        "api_key_env",
        "context_tokens",
        "max_output_tokens",
        "reasoning_effort",
        "vision",
        "timeout_s",
        "estimate_chars_per_token",
    ];
    let mut extra_keys: Vec<String> = v
        .as_table()
        .map(|t| {
            t.keys()
                .filter(|k| !known.contains(&k.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    extra_keys.sort();

    ModelEntry {
        name: name.to_string(),
        model_id: s("model_id"),
        base_url: s("base_url"),
        api_key_env: s("api_key_env"),
        context_tokens: n("context_tokens"),
        max_output_tokens: n("max_output_tokens"),
        reasoning_effort: s("reasoning_effort"),
        vision: b("vision"),
        timeout_s: n("timeout_s"),
        estimate_chars_per_token: n("estimate_chars_per_token"),
        extra_keys,
    }
}

fn global_int(model: Option<&toml::value::Table>, key: &str) -> Option<u64> {
    model
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_integer())
        .and_then(|v| u64::try_from(v).ok())
}

fn global_str(model: Option<&toml::value::Table>, key: &str) -> Option<String> {
    model
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Run the kernel's own `model --describe` so the panel shows the values
/// that are actually in effect. Best effort: on any failure the panel
/// just omits the block.
fn describe(config: &Path) -> Option<serde_json::Value> {
    let out = std::process::Command::new("model")
        .arg("--describe")
        .arg("--config")
        .arg(config)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

// ── save ────────────────────────────────────────────────────────────

/// Replace `path` atomically: temp file in the same directory (so the
/// rename stays on one filesystem), fsync'd before the rename, with the
/// original file's permissions carried over. The loop's stage binaries
/// read this file on every spawn, so a half-written config must never be
/// observable.
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".into());
    let tmp = dir.join(format!(".{file_name}.tmp"));

    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
    f.write_all(content.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
    drop(f);
    if let Ok(md) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, md.permissions());
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("替换 {} 失败: {e}", path.display()))?;
    Ok(())
}

pub fn save(cfg: &WebConfig, next: &ModelSettings) -> Result<SaveOutcome, String> {
    let path = config_path(cfg)?;
    let original = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    let before: toml::Value = original
        .parse()
        .map_err(|e| format!("config.toml 解析失败（未写入）: {e}"))?;

    validate(next, &before)?;

    let mut doc: DocumentMut = original
        .parse()
        .map_err(|e| format!("config.toml 解析失败（未写入）: {e}"))?;

    // Names of the sub-tables present right now (the global scalar keys
    // live in the same table and must not be mistaken for entries).
    let existing: Vec<String> = doc
        .get("model")
        .and_then(|i| i.as_table())
        .map(|t| {
            t.iter()
                .filter(|(_, v)| v.is_table())
                .map(|(k, _)| k.to_string())
                .collect()
        })
        .unwrap_or_default();

    {
        let model = doc
            .get_mut("model")
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| "config.toml 缺少 [model] 表".to_string())?;

        // Globals (only the five the kernel falls back to).
        set_opt_int(model, "max_output_tokens", next.globals.max_output_tokens);
        set_opt_str(model, "reasoning_effort", next.globals.reasoning_effort.as_deref());
        set_opt_int(model, "model_timeout_s", next.globals.model_timeout_s);
        set_opt_int(
            model,
            "estimate_chars_per_token",
            next.globals.estimate_chars_per_token,
        );
        set_opt_bool(model, "vision", next.globals.vision);

        // Entries removed in the form are removed from the file.
        for name in &existing {
            if !next.entries.iter().any(|e| &e.name == name) {
                model.remove(name);
            }
        }
        // Entries added or edited. Unknown keys inside an entry survive:
        // only the fields the panel owns are set or removed.
        for e in &next.entries {
            let needs_create = !model.get(&e.name).map(|i| i.is_table()).unwrap_or(false);
            if needs_create {
                model.insert(&e.name, Item::Table(Table::new()));
            }
            let t = model
                .get_mut(&e.name)
                .and_then(|i| i.as_table_mut())
                .ok_or_else(|| format!("无法写入 [model.\"{}\"]", e.name))?;
            set_opt_str(t, "model_id", e.model_id.as_deref());
            let base_url = e.base_url.as_deref().map(normalize_base_url);
            set_opt_str(t, "base_url", base_url.as_deref());
            set_opt_str(t, "api_key_env", e.api_key_env.as_deref());
            set_opt_int(t, "context_tokens", e.context_tokens);
            set_opt_int(t, "max_output_tokens", e.max_output_tokens);
            set_opt_str(t, "reasoning_effort", e.reasoning_effort.as_deref());
            set_opt_bool(t, "vision", e.vision);
            set_opt_int(t, "timeout_s", e.timeout_s);
            set_opt_int(t, "estimate_chars_per_token", e.estimate_chars_per_token);
        }
    }

    {
        let active = doc
            .get_mut("active")
            .and_then(|i| i.as_table_mut());
        match active {
            Some(a) => {
                a.insert("model", toml_edit::value(next.active.clone()));
            }
            None => {
                let mut a = Table::new();
                a.insert("model", toml_edit::value(next.active.clone()));
                doc.insert("active", Item::Table(a));
            }
        }
    }

    let rendered = doc.to_string();

    // Re-parse the rendered text and re-check the invariants the kernel
    // enforces (or silently punishes). A failure here means a bug in the
    // edit above, not user error — the original file is untouched.
    let after: toml::Value = rendered
        .parse()
        .map_err(|e| format!("内部错误：渲染后的配置无法解析（未写入）: {e}"))?;
    let names: Vec<String> = after
        .get("model")
        .and_then(|m| m.as_table())
        .map(|t| {
            t.iter()
                .filter(|(_, v)| v.is_table())
                .map(|(k, _)| k.to_string())
                .collect()
        })
        .unwrap_or_default();
    if !names.iter().any(|n| n == &next.active) {
        return Err(format!(
            "内部错误：`[active] model = \"{}\"` 在 [model] 中没有对应条目（未写入）",
            next.active
        ));
    }
    for (section, key) in LEGACY_KEYS {
        if after.get(section).and_then(|s| s.get(key)).is_some() {
            return Err(format!(
                "内部错误：配置里出现 legacy 键 [{section}] {key}（内核会 exit(1)，未写入）"
            ));
        }
    }

    // Back up, write, mirror. The backup is named `config.bak.toml` (not
    // `config.toml.bak`) so the kernel repo's own `.gitignore` rule
    // `config*.toml` keeps it untracked without touching the kernel tree.
    let backup = path.with_extension("bak.toml");
    std::fs::copy(&path, &backup)
        .map_err(|e| format!("备份 {} 失败（未写入）: {e}", backup.display()))?;

    let mut mirror_written = None;
    if let Some(mirror) = mirror_path(&path) {
        write_atomic(&path, &rendered)?;
        match write_atomic(&mirror, &rendered) {
            Ok(()) => mirror_written = Some(mirror.display().to_string()),
            Err(e) => {
                // The mirror is a safety net for the next sync, not the
                // live config; report it instead of failing the save.
                return Ok(SaveOutcome {
                    needs_loop_restart: diffs(&before, &after),
                    warnings: vec![format!(
                        "内核 config 已保存，但镜像 {} 写入失败：{e}（下次 sync.sh 可能回滚这次改动）",
                        mirror.display()
                    )],
                    backup: Some(backup.display().to_string()),
                    mirror: None,
                });
            }
        }
    } else {
        write_atomic(&path, &rendered)?;
    }

    let mut warnings = Vec::new();
    for e in &next.entries {
        if let Some(eff) = &e.reasoning_effort {
            if !EFFORT_VALUES.contains(&eff.as_str()) {
                warnings.push(format!(
                    "条目 {} 的 reasoning_effort = \"{eff}\" 不在内核认的取值 [{}] 里；\
                     内核不校验，会原样发给服务端",
                    e.name,
                    EFFORT_VALUES.join(", ")
                ));
            }
        }
    }

    Ok(SaveOutcome {
        needs_loop_restart: diffs(&before, &after),
        warnings,
        backup: Some(backup.display().to_string()),
        mirror: mirror_written,
    })
}

fn validate(next: &ModelSettings, before: &toml::Value) -> Result<(), String> {
    if next.entries.is_empty() {
        return Err("至少要保留一个模型条目".to_string());
    }
    if next.active.trim().is_empty() {
        return Err("活跃模型不能为空".to_string());
    }
    if !next.entries.iter().any(|e| e.name == next.active) {
        return Err(format!(
            "活跃模型 \"{}\" 不在条目列表里；内核会静默退回全部默认值",
            next.active
        ));
    }
    // Scalar keys at the `[model]` level (api, max_output_tokens, ...)
    // and sub-tables share one namespace: an entry named like an existing
    // scalar key cannot be written.
    let scalar_keys: Vec<String> = before
        .get("model")
        .and_then(|m| m.as_table())
        .map(|t| {
            t.iter()
                .filter(|(_, v)| !v.is_table())
                .map(|(k, _)| k.to_string())
                .collect()
        })
        .unwrap_or_default();
    for e in &next.entries {
        if e.name.trim().is_empty() {
            return Err("模型条目名不能为空".to_string());
        }
        if e.name.contains('"') || e.name.contains('\n') {
            return Err(format!("模型条目名含非法字符: {}", e.name));
        }
        if scalar_keys.contains(&e.name) || GLOBAL_KEYS.contains(&e.name.as_str()) {
            return Err(format!(
                "条目名 \"{}\" 与 [model] 下的全局键同名，TOML 里无法共存；请换一个名字",
                e.name
            ));
        }
        if let Some(url) = &e.base_url {
            if !url.trim().is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!(
                    "条目 {} 的 base_url 必须以 http:// 或 https:// 开头",
                    e.name
                ));
            }
        }
    }
    Ok(())
}

/// Which changes force a loop restart. The stage binaries re-read the
/// config on every spawn, so most keys apply on the next step; the loop
/// itself snapshots the context budget and the active entry at startup.
fn diffs(before: &toml::Value, after: &toml::Value) -> Vec<String> {
    let mut out = Vec::new();
    let s = |d: &toml::Value, sec: &str, key: &str| {
        d.get(sec)
            .and_then(|x| x.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let old_active = s(before, "active", "model").unwrap_or_default();
    let new_active = s(after, "active", "model").unwrap_or_default();
    if old_active != new_active {
        out.push(format!(
            "活跃模型已切换（{old_active} → {new_active}）：循环在启动时快照活跃条目，\
             需重启会话 loop 才一致"
        ));
    }

    let entries = |d: &toml::Value| -> BTreeMap<String, toml::Value> {
        d.get("model")
            .and_then(|m| m.as_table())
            .map(|t| {
                t.iter()
                    .filter(|(_, v)| v.is_table())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (b, a) = (entries(before), entries(after));
    for (name, av) in &a {
        match b.get(name) {
            None => out.push(format!("新增了模型条目 {name}")),
            Some(bv) => {
                if bv.get("context_tokens") != av.get("context_tokens") {
                    out.push(format!(
                        "条目 {name} 的 context_tokens 已修改：循环启动时按旧窗口算压缩阈值，\
                         需重启会话 loop"
                    ));
                }
            }
        }
    }
    for name in b.keys() {
        if !a.contains_key(name) {
            out.push(format!("删除了模型条目 {name}"));
        }
    }
    out
}

// ── toml_edit helpers ───────────────────────────────────────────────

fn set_opt_int(t: &mut Table, key: &str, v: Option<u64>) {
    match v {
        Some(n) => {
            t.insert(key, toml_edit::value(n as i64));
        }
        None => {
            t.remove(key);
        }
    }
}

/// The kernel builds URLs as `format!("{base_url}/v1/responses")`, so a
/// trailing slash would produce `//v1/...`; normalize it away on save.
fn normalize_base_url(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

fn set_opt_str(t: &mut Table, key: &str, v: Option<&str>) {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            t.insert(key, toml_edit::value(s));
        }
        None => {
            t.remove(key);
        }
    }
}

fn set_opt_bool(t: &mut Table, key: &str, v: Option<bool>) {
    match v {
        Some(b) => {
            t.insert(key, toml_edit::value(b));
        }
        None => {
            t.remove(key);
        }
    }
}

// ── connection probe ────────────────────────────────────────────────

/// Probe a candidate provider endpoint the way the kernel will use it:
/// `GET /v1/models` first (free), then a one-token `POST /v1/responses`,
/// then the `/v1/chat/completions` fallback the kernel takes on 404/405.
pub async fn probe(req: &ProbeRequest) -> ProbeResult {
    let base = req.base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return ProbeResult {
            ok: false,
            endpoint: String::new(),
            status: None,
            detail: "base_url 为空".to_string(),
        };
    }
    let key = match req
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(name) => match std::env::var(name) {
            Ok(v) => v,
            // Not fatal — an unauthenticated endpoint is legitimate —
            // but it is the most common cause of a 401, so say so now.
            Err(_) => {
                return ProbeResult {
                    ok: false,
                    endpoint: String::new(),
                    status: None,
                    detail: format!(
                        "环境变量 {name} 在 webui 服务进程里不存在；key 会以空串发出\
                         （服务端若要鉴权就会 401）。在 run-webui.sh 里 export 它再重启服务。"
                    ),
                }
            }
        },
        None => String::new(),
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                ok: false,
                endpoint: String::new(),
                status: None,
                detail: format!("HTTP 客户端构造失败: {e}"),
            }
        }
    };

    // 1) GET /v1/models — no compute, works on every OpenAI-compatible
    //    server (sglang, llama.cpp, vLLM, ...).
    let models_url = format!("{base}/v1/models");
    match client
        .get(&models_url)
        .bearer_auth(&key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            let n = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("data").and_then(|d| d.as_array()).map(|a| a.len()));
            let list = match n {
                Some(n) => format!("，列出 {n} 个模型"),
                None => String::new(),
            };
            return ProbeResult {
                ok: true,
                endpoint: "/v1/models".to_string(),
                status: Some(200),
                detail: format!("服务可达{list}"),
            };
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = truncate(resp.text().await.unwrap_or_default());
            // 404/405 means "no such endpoint" on this server: fall
            // through to the generation probe like the kernel does.
            if status != 404 && status != 405 {
                return ProbeResult {
                    ok: false,
                    endpoint: "/v1/models".to_string(),
                    status: Some(status),
                    detail: explain(status, &body),
                };
            }
        }
        Err(e) => {
            return ProbeResult {
                ok: false,
                endpoint: "/v1/models".to_string(),
                status: None,
                detail: format!("连不上 {models_url}：{}", transport_error(&e)),
            }
        }
    }

    // 2) minimal /v1/responses call (the kernel's primary path).
    let model = req
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    let resp_body = serde_json::json!({
        "model": model,
        "input": "ping",
        "max_output_tokens": 1,
        "store": false,
        "stream": false,
        "reasoning": { "effort": "none" },
    });
    let mut last = ProbeResult {
        ok: false,
        endpoint: "/v1/responses".to_string(),
        status: None,
        detail: "未尝试".to_string(),
    };
    for endpoint in ["/v1/responses", "/v1/chat/completions"] {
        let url = format!("{base}{endpoint}");
        let rb = if endpoint == "/v1/responses" {
            resp_body.clone()
        } else {
            serde_json::json!({
                "model": model,
                "messages": [{ "role": "user", "content": "ping" }],
                "max_tokens": 1,
                "stream": false,
            })
        };
        match client
            .post(&url)
            .bearer_auth(&key)
            .json(&rb)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                return ProbeResult {
                    ok: true,
                    endpoint: endpoint.to_string(),
                    status: Some(resp.status().as_u16()),
                    detail: format!("{endpoint} 返回 200（模型 {model} 可用）"),
                }
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = truncate(resp.text().await.unwrap_or_default());
                last = ProbeResult {
                    ok: false,
                    endpoint: endpoint.to_string(),
                    status: Some(status),
                    detail: explain(status, &body),
                };
                if status != 404 && status != 405 {
                    return last;
                }
            }
            Err(e) => {
                last = ProbeResult {
                    ok: false,
                    endpoint: endpoint.to_string(),
                    status: None,
                    detail: format!("请求 {url} 失败：{}", transport_error(&e)),
                };
            }
        }
    }
    last
}

/// Transport-level failures in words a user can act on: the raw reqwest
/// message ("error sending request for url ...") never says whether the
/// server is down, slow, or unreachable.
fn transport_error(e: &reqwest::Error) -> String {
    if e.is_connect() {
        format!("无法建立连接（服务没起？端口/隧道对不对？）：{e}")
    } else if e.is_timeout() {
        format!("超时：{e}")
    } else {
        e.to_string()
    }
}

fn truncate(s: String) -> String {
    let s = s.trim().to_string();
    if s.chars().count() > 400 {
        s.chars().take(400).collect::<String>() + "…"
    } else {
        s
    }
}

fn explain(status: u16, body: &str) -> String {
    let hint = match status {
        400 => "请求被拒（多半是模型名或参数不被接受）",
        401 | 403 => "鉴权失败（api_key_env 指向的环境变量是否配好）",
        404 => "路径或模型不存在",
        405 => "该方法不被支持",
        429 => "限流",
        500..=599 => "服务端错误（模型服务自己崩了？）",
        _ => "非预期状态",
    };
    if body.is_empty() {
        format!("HTTP {status}：{hint}")
    } else {
        format!("HTTP {status}：{hint}；响应体：{body}")
    }
}

// ── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature config carrying everything the writer must leave alone:
    /// a file header comment, a commented key, a hand-written dead key
    /// (`api`), an untouched section (`[hooks]`), and a multi-line string.
    const SAMPLE: &str = r#"# header comment
# second line

[model]
api = "responses"
max_output_tokens = 32768
reasoning_effort = "xhigh"

[model."local"]
model_id = "qwen3"
base_url = "http://127.0.0.1:30000"
api_key_env = "LLAMA_API_KEY"
context_tokens = 262144
vision = true

[paths]
# paths comment stays
sessions_root = "/abs/sessions"

[active]
model = "local"

[system_prompt]
text = """
multi
line
"""
"#;

    fn cfg_at(path: &Path) -> WebConfig {
        WebConfig {
            host: "127.0.0.1".into(),
            port: 8480,
            sessions_root: PathBuf::from("/tmp/sessions"),
            loop_cmd: vec!["rushi".into(), "run".into()],
            config_path: Some(path.to_path_buf()),
        }
    }

    fn write_sample(dir: &Path) -> PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(&p, SAMPLE).unwrap();
        p
    }

    fn settings_from(text: &str) -> ModelSettings {
        // Round-trips the sample through load()'s reader.
        let doc: toml::Value = text.parse().unwrap();
        let model = doc.get("model").and_then(|m| m.as_table());
        let entries = model
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| v.is_table())
                    .map(|(n, v)| entry_from_value(n, v))
                    .collect()
            })
            .unwrap_or_default();
        ModelSettings {
            active: doc
                .get("active")
                .and_then(|a| a.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            entries,
            globals: Globals {
                max_output_tokens: global_int(model, "max_output_tokens"),
                reasoning_effort: global_str(model, "reasoning_effort"),
                model_timeout_s: global_int(model, "model_timeout_s"),
                estimate_chars_per_token: global_int(model, "estimate_chars_per_token"),
                vision: model.and_then(|m| m.get("vision")).and_then(|v| v.as_bool()),
            },
            config_path: String::new(),
            mirror_path: None,
            key_env_present: BTreeMap::new(),
            effective: None,
            notes: Vec::new(),
        }
    }

    #[test]
    fn load_reads_entries_globals_and_active() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let m = load(&cfg_at(&p)).unwrap();
        assert_eq!(m.active, "local");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].name, "local");
        assert_eq!(m.entries[0].base_url.as_deref(), Some("http://127.0.0.1:30000"));
        assert_eq!(m.entries[0].context_tokens, Some(262144));
        assert_eq!(m.entries[0].vision, Some(true));
        assert_eq!(m.globals.max_output_tokens, Some(32768));
        assert_eq!(m.globals.reasoning_effort.as_deref(), Some("xhigh"));
        // The dead `api` key is reported, never treated as an entry.
        assert!(m.entries.iter().all(|e| e.name != "api"));
        assert!(m.notes.iter().any(|n| n.contains("api")));
    }

    #[test]
    fn save_edits_only_the_model_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let mut next = settings_from(SAMPLE);
        next.entries[0].base_url = Some("http://127.0.0.1:8080".to_string());
        next.entries[0].context_tokens = None; // cleared → key removed
        next.globals.max_output_tokens = Some(16384);

        let out = save(&cfg_at(&p), &next).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();

        assert!(after.contains(r#"base_url = "http://127.0.0.1:8080""#));
        assert!(!after.contains("context_tokens"), "cleared key must be removed");
        assert!(after.contains("max_output_tokens = 16384"));
        // Globals the form did not touch survive the round trip.
        assert!(after.contains(r#"reasoning_effort = "xhigh""#));
        // Untouched: comments, the dead key, other sections, formatting.
        assert!(after.starts_with("# header comment\n# second line\n"));
        assert!(after.contains(r#"api = "responses""#));
        assert!(after.contains("# paths comment stays"));
        assert!(after.contains("multi\nline"));
        assert!(after.contains("[system_prompt]"));
        // Backup written next to the config (name matches the kernel
        // repo's `config*.toml` ignore rule).
        assert!(out.backup.unwrap().ends_with("config.bak.toml"));
        assert!(dir.path().join("config.bak.toml").exists());
    }

    #[test]
    fn save_adds_and_removes_entries_and_keeps_active_valid() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let mut next = settings_from(SAMPLE);
        next.entries.push(ModelEntry {
            name: "cloud".into(),
            model_id: Some("gpt-5".into()),
            base_url: Some("https://api.example.com".into()),
            api_key_env: Some("OPENAI_API_KEY".into()),
            ..Default::default()
        });
        next.active = "cloud".into();
        // Removing the old entry while switching away from it is fine.
        next.entries.retain(|e| e.name != "local");
        save(&cfg_at(&p), &next).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        let doc: toml::Value = after.parse().unwrap();
        // toml_edit renders a bare-legal key unquoted: `[model.cloud]`.
        assert!(doc.get("model").and_then(|m| m.get("cloud")).is_some());
        assert!(doc.get("model").and_then(|m| m.get("local")).is_none());
        assert_eq!(
            doc.get("active").and_then(|a| a.get("model")).and_then(|v| v.as_str()),
            Some("cloud")
        );
        // The new sub-table lands inside the [model] group, not after
        // some later section.
        let cloud = after.find("cloud").expect("entry rendered");
        let paths = after.find("[paths]").expect("paths section kept");
        assert!(cloud < paths, "new entry must stay in the [model] group:\n{after}");
    }

    #[test]
    fn save_rejects_active_that_names_no_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let mut next = settings_from(SAMPLE);
        next.active = "nope".into();
        let err = save(&cfg_at(&p), &next).unwrap_err();
        assert!(err.contains("不在条目列表里"), "got: {err}");
        // The file is untouched.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE);
    }

    #[test]
    fn save_rejects_an_entry_named_like_a_global_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let mut next = settings_from(SAMPLE);
        next.entries.push(ModelEntry { name: "vision".into(), ..Default::default() });
        let err = save(&cfg_at(&p), &next).unwrap_err();
        assert!(err.contains("与 [model] 下的全局键同名"), "got: {err}");
    }

    #[test]
    fn save_rejects_a_base_url_without_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let mut next = settings_from(SAMPLE);
        next.entries[0].base_url = Some("127.0.0.1:30000".into());
        let err = save(&cfg_at(&p), &next).unwrap_err();
        assert!(err.contains("http://"), "got: {err}");
    }

    /// The kernel silently falls back to the default when a number is
    /// written as a string, so the wire format must reject it before it
    /// ever reaches the file.
    #[test]
    fn a_stringly_typed_number_never_deserializes() {
        let bad = r#"{"active":"local","entries":[{"name":"local","context_tokens":"262144"}],
                      "globals":{},"config_path":"","key_env_present":{}}"#;
        assert!(serde_json::from_str::<ModelSettings>(bad).is_err());
    }

    #[test]
    fn diffs_flag_what_needs_a_loop_restart() {
        let before: toml::Value = SAMPLE.parse().unwrap();
        let after: toml::Value = SAMPLE
            .replace(r#"model = "local""#, r#"model = "cloud""#)
            .replace("context_tokens = 262144", "context_tokens = 131072")
            .parse()
            .unwrap();
        let d = diffs(&before, &after);
        assert!(d.iter().any(|s| s.contains("活跃模型已切换")));
        assert!(d.iter().any(|s| s.contains("context_tokens")));
    }

    #[test]
    fn probe_reports_a_missing_key_env_without_network() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(probe(&ProbeRequest {
            base_url: "http://127.0.0.1:1".into(),
            api_key_env: Some("RUSHI_TEST_ABSENT_VAR".into()),
            model_id: None,
        }));
        assert!(!r.ok);
        assert!(r.detail.contains("RUSHI_TEST_ABSENT_VAR"), "got: {}", r.detail);
    }
}
