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
use toml_edit::{DocumentMut, Item, Key, Table};

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
    /// v0.5.47: the name this entry was loaded under. The form sends it
    /// back, so `save` can tell a RENAME (same entry, new name) from a
    /// delete + add: a rename keeps the entry's place in `[model]`
    /// instead of being appended at the end. Never set by `load`, absent
    /// for an entry the form just created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_name: Option<String>,
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
    /// For every `api_key_env` in use: is a key available for it — from
    /// the `config.secrets.toml` file the panel writes, or from the
    /// webui process's own environment? The key itself never reaches the
    /// browser — only this yes/no.
    pub key_env_present: BTreeMap<String, bool>,
    /// The subset of those that come from `config.secrets.toml` (a key
    /// pasted in the panel), so the form can offer "clear" for exactly
    /// those.
    #[serde(default)]
    pub key_env_stored: BTreeMap<String, bool>,
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
        .ok_or_else(|| {
            "the webui was started without --config, so the kernel config.toml cannot be \
             located"
                .to_string()
        })
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
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let doc: toml::Value = text
        .parse()
        .map_err(|e| format!("config.toml does not parse: {e}"))?;

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

    let secrets = load_secrets(cfg);
    let mut key_env_present = BTreeMap::new();
    let mut key_env_stored = BTreeMap::new();
    for e in &entries {
        let name = e
            .api_key_env
            .clone()
            .unwrap_or_else(|| "MODEL_API_KEY".to_string());
        let stored = secrets.contains_key(&name);
        key_env_stored.insert(name.clone(), stored);
        key_env_present.insert(name.clone(), stored || std::env::var(&name).is_ok());
    }

    // v0.5.47: no note for the dead `[model] api` key — the kernel never
    // reads it, so what the note said was noise. The key itself is still
    // left untouched on save.
    let mut notes = Vec::new();
    if !active.is_empty() && !entries.iter().any(|e| e.name == active) {
        notes.push(format!(
            "`[active] model = \"{active}\"` names no [model] entry — the kernel silently falls \
             back to every built-in default (base_url 127.0.0.1:8080 and so on)."
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
        key_env_stored,
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
        // A name read from the file has no "it used to be called" — that
        // only exists in the form's draft (v0.5.47).
        orig_name: None,
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
pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".into());
    let tmp = dir.join(format!(".{file_name}.tmp"));

    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    f.write_all(content.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    drop(f);
    if let Ok(md) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, md.permissions());
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(())
}

pub fn save(cfg: &WebConfig, next: &ModelSettings) -> Result<SaveOutcome, String> {
    let path = config_path(cfg)?;
    let original = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let before: toml::Value = original
        .parse()
        .map_err(|e| format!("config.toml does not parse (nothing written): {e}"))?;

    validate(next, &before)?;

    let mut doc: DocumentMut = original
        .parse()
        .map_err(|e| format!("config.toml does not parse (nothing written): {e}"))?;

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
            .ok_or_else(|| "config.toml has no [model] table".to_string())?;

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

        // Entries added, edited and removed. Unknown keys inside an entry
        // survive: only the fields the panel owns are set or removed.
        //
        // v0.5.47: a RENAME used to be a delete + an add, and because a
        // TOML table keeps insertion order the renamed entry landed at the
        // very end of `[model]` — the file looked rewritten for a one-word
        // edit. The form now sends `orig_name`, so a save that renames
        // rebuilds the entry order instead: every entry keeps the position
        // it had, the renamed one takes the place of the name it was
        // loaded under, a removed one disappears, and only a genuinely new
        // entry is appended (the one order the file cannot supply). A save
        // with no rename at all keeps the plain in-place path below, which
        // is what makes an unchanged save byte-identical.
        if renamed_entries(&existing, &next.entries).is_empty() {
            for name in &existing {
                if !next.entries.iter().any(|e| &e.name == name) {
                    model.remove(name);
                }
            }
            for e in &next.entries {
                let needs_create = !model.get(&e.name).map(|i| i.is_table()).unwrap_or(false);
                if needs_create {
                    model.insert(&e.name, Item::Table(Table::new()));
                }
                let t = model
                    .get_mut(&e.name)
                    .and_then(|i| i.as_table_mut())
                    .ok_or_else(|| format!("cannot write [model.\"{}\"]", e.name))?;
                apply_entry_fields(t, e);
            }
        } else {
            // `remove_entry`/`insert_formatted` rather than
            // `remove`/`insert`: the pair keeps each key's own
            // representation, so a file that writes `[model."alpha"]`
            // does not come back with the quotes stripped off.
            let mut taken: Vec<(Key, Item)> = Vec::new();
            for name in &existing {
                if let Some(kv) = model.remove_entry(name) {
                    taken.push(kv);
                }
            }
            let mut placed = vec![false; next.entries.len()];
            let mut order: Vec<(Key, Item)> = Vec::new();
            for (key, mut item) in taken {
                let old = key.get().to_string();
                // The entry that claims the name `old` used to have: the
                // one still called that, else the one renamed away from
                // it. Neither means the form dropped it.
                let slot = next
                    .entries
                    .iter()
                    .position(|e| e.name == old)
                    .or_else(|| {
                        next.entries
                            .iter()
                            .position(|e| e.orig_name.as_deref() == Some(old.as_str()))
                    });
                let Some(i) = slot else { continue };
                // `taken` holds the sub-tables of `existing`, nothing else.
                if let Some(t) = item.as_table_mut() {
                    apply_entry_fields(t, &next.entries[i]);
                }
                placed[i] = true;
                let name = next.entries[i].name.clone();
                let key = if name == old { key } else { renamed_key(&key, &name) };
                order.push((key, item));
            }
            for (i, e) in next.entries.iter().enumerate() {
                if placed[i] {
                    continue;
                }
                let mut t = Table::new();
                apply_entry_fields(&mut t, e);
                order.push((Key::new(&e.name), Item::Table(t)));
            }
            for (key, item) in order {
                model.insert_formatted(&key, item);
            }
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
        .map_err(|e| format!("internal error: the rendered config does not parse (nothing written): {e}"))?;
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
            "internal error: `[active] model = \"{}\"` names no [model] entry (nothing written)",
            next.active
        ));
    }
    for (section, key) in LEGACY_KEYS {
        if after.get(section).and_then(|s| s.get(key)).is_some() {
            return Err(format!(
                "internal error: the config would carry the legacy key [{section}] {key}, \
                 which makes the kernel exit(1) (nothing written)"
            ));
        }
    }

    // Back up, write, mirror. The backup is named `config.bak.toml` (not
    // `config.toml.bak`) so the kernel repo's own `.gitignore` rule
    // `config*.toml` keeps it untracked without touching the kernel tree.
    let backup = path.with_extension("bak.toml");
    std::fs::copy(&path, &backup)
        .map_err(|e| format!("cannot back up to {} (nothing written): {e}", backup.display()))?;

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
                        "the kernel config was saved, but the mirror {} could not be written: \
                         {e} (the next sync.sh may roll this change back)",
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
                    "entry {} sets reasoning_effort = \"{eff}\", which is not one of the values \
                     the kernel knows [{}]; the kernel does not validate it and sends it to the \
                     provider verbatim",
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

/// v0.5.47: `(orig_name, name)` for every entry the form renamed — the
/// name it was loaded under exists in the file, the new one differs.
/// Empty for the ordinary "edited some fields" save.
fn renamed_entries(existing: &[String], entries: &[ModelEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter_map(|e| {
            let orig = e.orig_name.as_deref()?;
            if orig.is_empty() || orig == e.name || e.name.trim().is_empty() {
                return None;
            }
            existing
                .iter()
                .any(|x| x == orig)
                .then(|| (orig.to_string(), e.name.clone()))
        })
        .collect()
}

/// The key for a renamed entry, written the way the file wrote the old
/// one: a quoted key stays quoted (a bare-safe name like `alpha` would
/// otherwise lose its quotes and read as a rewrite instead of a rename).
fn renamed_key(old: &Key, name: &str) -> Key {
    let repr = old.display_repr();
    let text = if repr.starts_with('"') || repr.starts_with('\'') {
        format!("\"{name}\"")
    } else {
        name.to_string()
    };
    text.parse().unwrap_or_else(|_| Key::new(name))
}

/// The fields the panel owns inside one `[model."…"]` entry. Unknown keys
/// survive: only these are set or removed.
fn apply_entry_fields(t: &mut Table, e: &ModelEntry) {
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

fn validate(next: &ModelSettings, before: &toml::Value) -> Result<(), String> {
    if next.entries.is_empty() {
        return Err("keep at least one model entry".to_string());
    }
    if next.active.trim().is_empty() {
        return Err("the active model must not be empty".to_string());
    }
    if !next.entries.iter().any(|e| e.name == next.active) {
        return Err(format!(
            "active model \"{}\" is not one of the entries; the kernel would silently fall back \
             to every built-in default",
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
    let mut seen: Vec<&str> = Vec::new();
    for e in &next.entries {
        if e.name.trim().is_empty() {
            return Err("an entry name must not be empty".to_string());
        }
        if seen.contains(&e.name.as_str()) {
            return Err(format!(
                "two entries are named \"{}\" — TOML cannot hold both; pick another name",
                e.name
            ));
        }
        seen.push(&e.name);
        if e.name.contains('"') || e.name.contains('\n') {
            return Err(format!("entry name has an illegal character: {}", e.name));
        }
        if scalar_keys.contains(&e.name) || GLOBAL_KEYS.contains(&e.name.as_str()) {
            return Err(format!(
                "entry \"{}\" collides with a global [model] key — TOML cannot hold both; pick \
                 another name",
                e.name
            ));
        }
        if let Some(url) = &e.base_url {
            if !url.trim().is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!(
                    "entry {} needs a base_url starting with http:// or https://",
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
            "active model switched ({old_active} -> {new_active}): a loop snapshots the active \
             entry at startup, so restart the session's loop"
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
            None => out.push(format!("added model entry {name}")),
            Some(bv) => {
                if bv.get("context_tokens") != av.get("context_tokens") {
                    out.push(format!(
                        "entry {name} changed context_tokens: the loop computes its compaction \
                         threshold from the window it snapshotted at startup, so restart the \
                         session's loop"
                    ));
                }
            }
        }
    }
    for name in b.keys() {
        if !a.contains_key(name) {
            out.push(format!("removed model entry {name}"));
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
pub async fn probe(cfg: &WebConfig, req: &ProbeRequest) -> ProbeResult {
    let base = req.base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return ProbeResult {
            ok: false,
            endpoint: String::new(),
            status: None,
            detail: "base_url is empty".to_string(),
        };
    }
    let key = match req
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(name) => match key_for(cfg, name) {
            Some(v) => v,
            // Not fatal — an unauthenticated endpoint is legitimate —
            // but it is the most common cause of a 401, so say so now.
            None => {
                return ProbeResult {
                    ok: false,
                    endpoint: String::new(),
                    status: None,
                    detail: format!(
                        "no key for {name}: paste it in the panel (it is stored in \
                         config.secrets.toml) or export {name} before starting the webui. \
                         Without one the key goes out empty and a server that authenticates \
                         answers 401."
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
                detail: format!("cannot build an HTTP client: {e}"),
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
                Some(n) => format!(", {n} model(s) listed"),
                None => String::new(),
            };
            return ProbeResult {
                ok: true,
                endpoint: "/v1/models".to_string(),
                status: Some(200),
                detail: format!("reachable{list}"),
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
                detail: format!("cannot reach {models_url}: {}", transport_error(&e)),
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
        detail: "not attempted".to_string(),
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
                    detail: format!("{endpoint} answered 200 (model {model} works)"),
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
                    detail: format!("request to {url} failed: {}", transport_error(&e)),
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
        format!("cannot connect (is the server up? right port/tunnel?): {e}")
    } else if e.is_timeout() {
        format!("timed out: {e}")
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
        400 => "the request was rejected (usually an unknown model name or an unsupported parameter)",
        401 | 403 => {
            "authentication failed (paste the key in the panel, or export the env var named by \
             api_key_env, then restart the server)"
        }
        404 => "no such path or model",
        405 => "that method is not supported here",
        429 => "rate limited",
        500..=599 => "server error (did the model service crash?)",
        _ => "unexpected status",
    };
    if body.is_empty() {
        format!("HTTP {status}: {hint}")
    } else {
        format!("HTTP {status}: {hint}; body: {body}")
    }
}

/// The config's `[active].model` — what a session without a choice of its
/// own follows, and the fallback shown on its card.
pub fn active_model(cfg: &WebConfig) -> Option<String> {
    active_model_in(cfg.config_path.as_deref()?)
}

/// `active_model` for an explicit config path (the per-session config a
/// loop was pinned to).
pub fn active_model_in(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: toml::Value = text.parse().ok()?;
    doc.get("active")
        .and_then(|a| a.get("model"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

// ── pasted keys ─────────────────────────────────────────────────────
//
// The kernel reads a key only from the environment: its config has
// `api_key_env` (a NAME) and nothing else, and `bin/model` falls back to
// an empty string when that name is unset. Pasting a key into the panel
// therefore stores it here, and the webui injects it into every loop it
// spawns under that same name — no kernel change, and the key never
// travels back to the browser.

const SECRETS_HEADER: &str = "\
# Provider keys pasted in the webui's model panel.
#
# The kernel never reads this file. The webui injects each value into
# every loop it spawns, under the env var name the entry's `api_key_env`
# declares (a value here wins over an env var of the same name).
# Keep it out of version control: the kernel repo's `config*.toml`
# ignore rule already covers this file name.

";

/// `<config dir>/config.secrets.toml`.
pub fn secrets_path(cfg: &WebConfig) -> Option<PathBuf> {
    let dir = cfg.config_path.as_ref()?.parent()?;
    Some(dir.join("config.secrets.toml"))
}

pub fn load_secrets(cfg: &WebConfig) -> BTreeMap<String, String> {
    let Some(path) = secrets_path(cfg) else {
        return BTreeMap::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    let Ok(doc) = text.parse::<toml::Value>() else {
        return BTreeMap::new();
    };
    doc.get("env")
        .and_then(|e| e.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Store (or, with `None`/empty, delete) one key. The file is written
/// atomically and left mode 0600.
pub fn set_secret(cfg: &WebConfig, name: &str, value: Option<&str>) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("api_key_env is empty: name the environment variable first".to_string());
    }
    let path = secrets_path(cfg)
        .ok_or_else(|| "cannot locate config.secrets.toml (no --config)".to_string())?;

    let mut map = load_secrets(cfg);
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => {
            map.insert(name.to_string(), v.to_string());
        }
        None => {
            map.remove(name);
        }
    }
    if map.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }

    let mut doc = DocumentMut::new();
    let mut env = Table::new();
    for (k, v) in &map {
        env.insert(k, toml_edit::value(v));
    }
    doc.insert("env", Item::Table(env));
    let rendered = format!("{SECRETS_HEADER}{doc}");
    write_atomic(&path, &rendered)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The value to send for the env var `name`: the secrets file first (an
/// explicit choice made in the panel), then the process environment.
pub fn key_for(cfg: &WebConfig, name: &str) -> Option<String> {
    load_secrets(cfg)
        .get(name)
        .cloned()
        .or_else(|| std::env::var(name).ok())
}

// ── provider model list ─────────────────────────────────────────────

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FetchRequest {
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchedModel {
    pub id: String,
    /// Reported window, when the provider exposes one. sglang returns
    /// `max_model_len`; OpenAI-compatible servers usually report nothing.
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchResult {
    pub ok: bool,
    pub models: Vec<FetchedModel>,
    pub detail: String,
}

/// `GET {base_url}/v1/models` — the one piece of provider metadata that
/// is actually discoverable at runtime. Reasoning capabilities are NOT
/// (the reference harness, dsh/pi-ai, ships a generated per-model catalog
/// with an explicit `thinkingLevelMap` instead of probing).
pub async fn fetch_models(cfg: &WebConfig, req: &FetchRequest) -> FetchResult {
    let base = req.base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return FetchResult {
            ok: false,
            models: Vec::new(),
            detail: "base_url is empty".to_string(),
        };
    }
    let key = req
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|name| key_for(cfg, name))
        .unwrap_or_default();

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return FetchResult {
                ok: false,
                models: Vec::new(),
                detail: format!("cannot build an HTTP client: {e}"),
            }
        }
    };

    let url = format!("{base}/v1/models");
    match client.get(&url).bearer_auth(&key).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            match parse_model_list(&body) {
                Ok(models) => {
                    let n = models.len();
                    FetchResult {
                        ok: true,
                        models,
                        detail: format!("{n} model(s)"),
                    }
                }
                Err(e) => FetchResult {
                    ok: false,
                    models: Vec::new(),
                    detail: format!("{url} did not return a model list: {e}"),
                },
            }
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = truncate(resp.text().await.unwrap_or_default());
            FetchResult {
                ok: false,
                models: Vec::new(),
                detail: explain(status, &body),
            }
        }
        Err(e) => FetchResult {
            ok: false,
            models: Vec::new(),
            detail: format!("cannot reach {url}: {}", transport_error(&e)),
        },
    }
}

/// Parse an OpenAI-shaped `/v1/models` body. Ids keep the bare name (a
/// trailing slash is dropped — sglang reports `Qwen3...-v2/`); a window
/// is picked up from whichever key the provider uses.
fn parse_model_list(body: &str) -> Result<Vec<FetchedModel>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let arr = doc
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "no `data` array".to_string())?;
    let mut models: Vec<FetchedModel> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|i| i.as_str())?;
            let id = id.trim().trim_end_matches('/').to_string();
            if id.is_empty() {
                return None;
            }
            let context_window = ["max_model_len", "context_window", "context_length"]
                .iter()
                .find_map(|k| m.get(*k).and_then(|v| v.as_u64()));
            Some(FetchedModel { id, context_window })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

// ── per-session config ──────────────────────────────────────────────

/// Rewrite `[active] model` — and, when given, that entry's
/// `reasoning_effort` — in a copy of the kernel config, leaving every
/// other byte alone.
///
/// A per-session model cannot ride on the `$MODEL` env var: only
/// `bin/model`, `bin/assemble` and `bin/compact` read it, while the loop
/// itself snapshots its context budget from `[active]` at startup — the
/// request would use B while compaction math used A. `$CONFIG` pointing
/// at a config whose `[active]` is B is the one channel the loop and
/// every stage follow together (the webui already pins `$CONFIG` per
/// loop). Reasoning effort is per-entry in the kernel, so a per-session
/// effort rides along in the same file.
pub fn session_config(
    original: &str,
    entry: Option<&str>,
    effort: Option<&str>,
) -> Result<String, String> {
    let mut doc: DocumentMut = original
        .parse()
        .map_err(|e| format!("config.toml does not parse: {e}"))?;

    if let Some(entry) = entry {
        let active = doc.get_mut("active").and_then(|i| i.as_table_mut());
        match active {
            Some(a) => {
                a.insert("model", toml_edit::value(entry));
            }
            None => {
                let mut a = Table::new();
                a.insert("model", toml_edit::value(entry));
                doc.insert("active", Item::Table(a));
            }
        }
    }

    if let Some(effort) = effort {
        // The entry the session will actually run: its own pick, else
        // whatever `[active]` names.
        let target = entry.map(str::to_string).or_else(|| {
            doc.get("active")
                .and_then(|a| a.get("model"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        });
        if let Some(target) = target {
            let model = doc
                .get_mut("model")
                .and_then(|i| i.as_table_mut())
                .ok_or_else(|| "config.toml has no [model] table".to_string())?;
            let needs_create = !model.get(&target).map(|i| i.is_table()).unwrap_or(false);
            if needs_create {
                model.insert(&target, Item::Table(Table::new()));
            }
            let t = model
                .get_mut(&target)
                .and_then(|i| i.as_table_mut())
                .ok_or_else(|| format!("cannot write [model.\"{target}\"]"))?;
            t.insert("reasoning_effort", toml_edit::value(effort));
        }
    }

    Ok(doc.to_string())
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
            key_env_stored: BTreeMap::new(),
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
        // The dead `api` key never becomes an entry, and (v0.5.47) it is
        // not reported as a note either — the kernel never reads it.
        assert!(m.entries.iter().all(|e| e.name != "api"));
        assert!(m.notes.is_empty(), "unexpected notes: {:?}", m.notes);
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

    /// Two entries in a known order, so a rename's POSITION is checkable.
    const TWO: &str = r#"[model]
max_output_tokens = 32768

[model."alpha"]
model_id = "a"
base_url = "http://127.0.0.1:1000"

[model."beta"]
model_id = "b"
base_url = "http://127.0.0.1:2000"

[active]
model = "alpha"
"#;

    fn write_two(dir: &Path) -> PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(&p, TWO).unwrap();
        p
    }

    fn entry_names(text: &str) -> Vec<String> {
        let doc: toml::Value = text.parse().unwrap();
        doc.get("model")
            .and_then(|m| m.as_table())
            .map(|t| {
                t.iter()
                    .filter(|(_, v)| v.is_table())
                    .map(|(k, _)| k.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn save_renames_an_entry_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_two(dir.path());
        let mut next = settings_from(TWO);
        next.entries[0].orig_name = Some("alpha".to_string());
        next.entries[0].name = "alpha-x".to_string();
        next.active = "alpha-x".to_string();

        save(&cfg_at(&p), &next).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();

        // The rename lands where the old name was: the file is the sample
        // with the name substituted, nothing else moved.
        assert_eq!(after, TWO.replace("alpha", "alpha-x"));
        assert_eq!(entry_names(&after), vec!["alpha-x", "beta"]);
    }

    #[test]
    fn save_rename_keeps_the_other_entries_and_appends_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_two(dir.path());
        let mut next = settings_from(TWO);
        // alpha -> alpha-x (stays first), beta removed, gamma added.
        next.entries[0].orig_name = Some("alpha".to_string());
        next.entries[0].name = "alpha-x".to_string();
        next.entries.remove(1);
        let mut gamma = ModelEntry {
            name: "gamma".to_string(),
            base_url: Some("http://127.0.0.1:3000".to_string()),
            ..Default::default()
        };
        gamma.orig_name = None;
        next.entries.push(gamma);
        next.active = "alpha-x".to_string();

        save(&cfg_at(&p), &next).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();

        assert_eq!(entry_names(&after), vec!["alpha-x", "gamma"]);
        assert!(!after.contains("[model.\"beta\"]"));
        // A brand-new entry is written with a bare key when the name
        // allows it — the same shape `[model.deepseek-flash]` has in a
        // real config. Only a RENAME inherits the old key's quoting.
        assert!(after.contains("[model.gamma]"), "{after}");
        assert!(after.contains("[model.\"alpha-x\"]"), "{after}");
    }

    #[test]
    fn save_rejects_two_entries_with_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_two(dir.path());
        let mut next = settings_from(TWO);
        let mut dup = next.entries[0].clone();
        dup.orig_name = None;
        next.entries.push(dup);

        let err = save(&cfg_at(&p), &next).unwrap_err();
        assert!(err.contains("two entries are named"), "{err}");
        // Nothing was written.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), TWO);
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
        assert!(err.contains("is not one of the entries"), "got: {err}");
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
        assert!(err.contains("collides with a global [model] key"), "got: {err}");
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
        assert!(d.iter().any(|s| s.contains("active model switched")));
        assert!(d.iter().any(|s| s.contains("context_tokens")));
    }

    #[test]
    fn parse_model_list_trims_the_shared_id_shape() {
        // The exact shape this machine's sglang returns.
        let body = r#"{"object":"list","data":[
            {"id":"Qwen3.8-27B-NVFP4-RTX5090-v2/","object":"model","created":1,
             "owned_by":"sglang","root":"x/","parent":null,"max_model_len":262144}]}"#;
        let m = parse_model_list(body).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].id, "Qwen3.8-27B-NVFP4-RTX5090-v2");
        assert_eq!(m[0].context_window, Some(262144));
    }

    #[test]
    fn parse_model_list_accepts_a_plain_openai_payload() {
        let body = r#"{"object":"list","data":[{"id":"gpt-5","object":"model"},{"id":"o3"}]}"#;
        let m = parse_model_list(body).unwrap();
        assert_eq!(m.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["gpt-5", "o3"]);
        assert!(m.iter().all(|x| x.context_window.is_none()));
        assert!(parse_model_list("not json").is_err());
        assert!(parse_model_list(r#"{"data":"nope"}"#).is_err());
    }

    #[test]
    fn session_config_swaps_only_the_active_entry() {
        let out = session_config(SAMPLE, Some("cloud"), None).unwrap();
        let doc: toml::Value = out.parse().unwrap();
        assert_eq!(
            doc.get("active").and_then(|a| a.get("model")).and_then(|v| v.as_str()),
            Some("cloud")
        );
        // Everything else is byte-identical to the source.
        let expected = SAMPLE.replace(r#"model = "local""#, r#"model = "cloud""#);
        assert_eq!(out, expected);
    }

    #[test]
    fn session_config_can_pin_a_reasoning_effort() {
        let out = session_config(SAMPLE, Some("cloud"), Some("low")).unwrap();
        let doc: toml::Value = out.parse().unwrap();
        // The entry the session runs carries the effort...
        assert_eq!(
            doc.get("model")
                .and_then(|m| m.get("cloud"))
                .and_then(|e| e.get("reasoning_effort"))
                .and_then(|v| v.as_str()),
            Some("low")
        );
        assert_eq!(
            doc.get("active").and_then(|a| a.get("model")).and_then(|v| v.as_str()),
            Some("cloud")
        );
        // ...and the shared config is untouched: the original sample's
        // entry has no effort key at all.
        let orig: toml::Value = SAMPLE.parse().unwrap();
        assert!(orig
            .get("model")
            .and_then(|m| m.get("local"))
            .and_then(|e| e.get("reasoning_effort"))
            .is_none());

        // Effort alone (no entry pick) applies to whatever `[active]`
        // names.
        let out = session_config(SAMPLE, None, Some("high")).unwrap();
        let doc: toml::Value = out.parse().unwrap();
        assert_eq!(
            doc.get("model")
                .and_then(|m| m.get("local"))
                .and_then(|e| e.get("reasoning_effort"))
                .and_then(|v| v.as_str()),
            Some("high")
        );
    }

    #[test]
    fn probe_reports_a_missing_key_env_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(probe(
            &cfg_at(&p),
            &ProbeRequest {
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: Some("RUSHI_TEST_ABSENT_VAR".into()),
                model_id: None,
            },
        ));
        assert!(!r.ok);
        assert!(r.detail.contains("RUSHI_TEST_ABSENT_VAR"), "got: {}", r.detail);
    }

    #[test]
    fn a_pasted_key_is_stored_0600_and_preferred() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_sample(dir.path());
        let cfg = cfg_at(&p);

        // Nothing stored, nothing in the env.
        assert!(key_for(&cfg, "RUSHI_TEST_KEY").is_none());

        set_secret(&cfg, "RUSHI_TEST_KEY", Some("sk-abc")).unwrap();
        let secrets = secrets_path(&cfg).unwrap();
        assert!(secrets.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secrets).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secrets must not be world readable");
        }
        assert_eq!(key_for(&cfg, "RUSHI_TEST_KEY").as_deref(), Some("sk-abc"));
        assert_eq!(load_secrets(&cfg).len(), 1);

        // The secrets file wins over an env var of the same name.
        std::env::set_var("RUSHI_TEST_KEY", "from-env");
        assert_eq!(key_for(&cfg, "RUSHI_TEST_KEY").as_deref(), Some("sk-abc"));
        std::env::remove_var("RUSHI_TEST_KEY");

        // A second key is added without dropping the first, and clearing
        // the last one removes the file.
        set_secret(&cfg, "RUSHI_TEST_OTHER", Some("sk-def")).unwrap();
        assert_eq!(load_secrets(&cfg).len(), 2);
        set_secret(&cfg, "RUSHI_TEST_KEY", None).unwrap();
        assert_eq!(load_secrets(&cfg).len(), 1);
        set_secret(&cfg, "RUSHI_TEST_OTHER", Some("")).unwrap();
        assert!(!secrets.exists(), "an empty store removes the file");

        // The kernel config itself is never touched by any of this.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE);
    }
}
