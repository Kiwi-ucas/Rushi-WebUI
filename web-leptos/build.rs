//! Injects the webui build version into the frontend at compile time.
//!
//! Single source of truth for the version string shown in the sidebar
//! header ("Rushi vX.Y.Z") and the boot-time `console.log` marker in
//! pile.rs — previously a hand-edited literal that drifted behind the
//! tree. Resolution order:
//!
//!   1. `RUSHI_WEBUI_VERSION` env var — explicit override (CI or a
//!      release build can pin any string); used verbatim.
//!   2. The most recent `vX.Y.Z` commit subject in git history — the
//!      repo's versioning convention is versioned commit subjects
//!      ("v0.5.39: …"); there are no git tags to describe from.
//!   3. `dev-<short-sha>` when inside a git checkout without a
//!      versioned commit yet.
//!   4. "dev" as a last resort (no git available).
//!
//! In-progress bump: when the worktree has **uncommitted** changes
//! (`git status --porcelain` non-empty), the build is "the next
//! version being worked on", so the versioned subject is bumped by one
//! on its last numeric component (v0.5.39 → v0.5.40). A clean tree at
//! a `vX.Y.Z` commit reads that version; committing `vX.Y.(Z+1): …`
//! then reads the bumped value from git. No hand-editing, so the
//! string can't drift.
//!
//! Re-run conditions: the env override, this file, the crate's source
//! (any version commit carries code, so a recompute happens), and the
//! repo's .git directory itself when present (catches version-only
//! commits and commit/checkout switches).

fn main() {
    println!("cargo:rerun-if-env-changed=RUSHI_WEBUI_VERSION");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();

    // Watch the git directory when this checkout has one, so a
    // version-tagged commit (or a checkout switch) recomputes the
    // version even without a source change.
    if std::path::Path::new(&manifest).join("../.git").exists() {
        println!("cargo:rerun-if-changed=../.git");
    }

    // An explicit override wins verbatim — no bumping, no surprises.
    if let Some(v) = std::env::var("RUSHI_WEBUI_VERSION").ok() {
        emit(&v);
        return;
    }

    match git_subject_version(&manifest) {
        Some(base) => {
            // Uncommitted work means "the version being worked on next".
            let version = if git_is_dirty(&manifest) {
                bump_patch(&base)
            } else {
                base
            };
            emit(&version);
        }
        None => emit(&dev_fallback(&manifest)),
    }
}

/// Publish the resolved version to the compiler and echo it as a build
/// warning so the resolved value is visible in every cargo output.
fn emit(version: &str) {
    println!("cargo:rustc-env=RUSHI_WEBUI_VERSION={version}");
    println!("cargo:warning=rushi-webui build version: {version}");
}

/// True when the worktree has uncommitted **tracked** changes
/// (`git status --porcelain` lines other than `??` untracked entries).
/// Untracked-only noise (local dirs, scratch files) does NOT count as
/// in-progress version work, so a tree that is clean modulo untracked
/// files still reads its versioned commit. Used to bump an in-progress
/// version. A missing/non-zero exit (no git, or not a checkout) means
/// we can't tell, so treat it as clean (no bump).
fn git_is_dirty(manifest: &str) -> bool {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(manifest)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| !l.starts_with("??")),
        _ => false,
    }
}

/// Bump the last *purely numeric* component of a `vX.Y.Z`-style version
/// (v0.5.39 -> v0.5.40). Returns the input unchanged when it isn't a
/// clean three-numeric-segment version, so unusual tokens (suffixes,
/// "dev-…") are never mangled.
fn bump_patch(v: &str) -> String {
    let digits: Vec<&str> = v
        .trim_start_matches('v')
        .split('.')
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if digits.len() != 3 {
        return v.to_string();
    }
    let n: u64 = digits[2].parse().unwrap_or(0);
    format!("v{}.{}.{}", digits[0], digits[1], n + 1)
}

/// Most recent commit subject starting with `vX.Y.Z` (subjects are
/// newest-first; tolerate a trailing letter, e.g. `v0.5.14b`).
fn git_subject_version(manifest: &str) -> Option<String> {
    let out = git_run(manifest, &["log", "-n", "400", "--format=%s"])?;
    for line in out.lines() {
        let token = line.split_whitespace().next()?;
        if let Some(v) = parse_version_token(token) {
            return Some(v.to_string());
        }
    }
    None
}

fn parse_version_token(t: &str) -> Option<String> {
    let rest = t.strip_prefix('v')?;
    // The token may carry a trailing colon / description
    // ("v0.5.39: one-line …"); keep only the leading version part.
    let ver: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || c.is_ascii_lowercase())
        .collect();
    let looks_like =
        !ver.is_empty() && ver.contains('.') && ver.chars().any(|c| c.is_ascii_digit());
    if looks_like {
        Some(format!("v{ver}"))
    } else {
        None
    }
}

fn dev_fallback(manifest: &str) -> String {
    if let Some(sha) = git_run(manifest, &["rev-parse", "--short", "HEAD"]) {
        format!("dev-{}", sha.trim())
    } else {
        "dev".to_string()
    }
}

fn git_run(manifest: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(manifest)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}
