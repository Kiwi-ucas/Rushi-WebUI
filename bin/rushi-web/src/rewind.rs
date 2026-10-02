//! Read-only history-tree projection over a session's `events.jsonl`
//! (`GET /api/sessions/{id}/rewind`).
//!
//! The projection mirrors the kernel's active-path semantics
//! (`rushi/crates/rushi/src/rewind.rs`): a `rewind` event at log line `S`
//! with `target_seq` `T` states that the model context at `S` equals the
//! context at `T_eff` — `T` for mode `on`, `T - 1` for mode `before`. The
//! events in the open span `(T_eff, S)` leave the active path; they stay in
//! the append-only log, so every branch is preserved and a later rewind can
//! re-enter it. This module is a pure, read-only *view* of that structure:
//! it never writes.
//!
//! The tree is built in one scan with a cursor + parent map:
//!
//!   * a `user_message` attaches to the current cursor and becomes the new
//!     cursor (a sequential round);
//!   * a valid `rewind(S, T, mode)` moves the cursor back to the round that
//!     contains `T_eff`.
//!
//! The fork is simply the second time a round gets a child: the next round
//! attaches to the old target, while the previously-next round stays as the
//! abandoned sibling. The active path is the parent chain of the final
//! cursor, which is provably the same set of rounds the kernel's
//! `active_ranges(total, rewinds)` keeps (asserted in the tests below against
//! a ported reference implementation, over the kernel's own fixtures).

use std::collections::HashSet;

use serde::Serialize;
use serde_json::Value;

/// One node = one user message = one loop round.
#[derive(Debug, Clone, Serialize)]
pub struct RewindNode {
    /// 1-based log line of the round's `user_message` — the `target_seq` a
    /// rewind to this node writes.
    pub seq: u64,
    /// 1-based round index (1 + the count of earlier user messages).
    pub round: u64,
    pub ts: String,
    pub summary: String,
    /// Non-`ext_status` events folded into this round (the agent's work).
    pub events: u64,
    /// `"active"` (on the active path) or `"abandoned"` (a masked fork).
    pub state: String,
    /// The "you are here" round (the final cursor).
    pub current: bool,
    /// A `user_message_retract` targeted this round's message id.
    pub retracted: bool,
    /// How the context is reconstructed if the user rewinds to this
    /// round (the kernel's rule, `docs/rewind-plugin-plan.md` 11.3).
    pub restore: Restore,
    pub children: Vec<RewindNode>,
}

/// One parsed `rewind` marker, at its 1-based log line.
#[derive(Debug, Clone, Serialize)]
pub struct RewindMarker {
    pub seq: u64,
    pub target_seq: u64,
    pub mode: String,
}

/// One compaction boundary as the *tree* reports it: every
/// `compaction_summary` marker of the log (branch markers included),
/// with the handoff version it names. The boundary that *governs* the
/// context is [`boundary_on_active_path`].
#[derive(Debug, Clone, Serialize)]
pub struct Boundary {
    pub seq: u64,
    pub first_kept_seq: u64,
    /// The handoff version the marker names (0 on a legacy marker).
    pub version: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewindTree {
    pub session: String,
    pub total_events: u64,
    pub total_rounds: u64,
    pub roots: Vec<RewindNode>,
    pub rewinds: Vec<RewindMarker>,
    pub boundaries: Vec<Boundary>,
    /// The "you are here" round, if the log has any.
    pub current_seq: Option<u64>,
    /// Where the next context assembly ends (equals `current_seq`).
    pub pending_from: Option<u64>,
    /// The log's last structural event is a rewind marker: the session
    /// settled at the target with no round after it.
    pub settled: bool,
    /// The markers the kernel's projection drops on this log (the P4
    /// pair-stranding guard), outermost first. Non-empty means a
    /// rewind the user made did **not** take effect.
    pub ignored: Vec<IgnoredMarker>,
    /// The ignored marker whose seq is the log's last rewind marker:
    /// the "your last rewind did not take effect" notice (D-C).
    pub tail_ignored: Option<IgnoredMarker>,
}

struct Round {
    seq: u64,
    ts: String,
    content: String,
    id: Option<String>,
    events: u64,
    retracted: bool,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// Build the history tree for a session from its event list (chronological,
/// one element per non-empty log line — element `i` is 1-based line `i + 1`,
/// which is the kernel's `seq`).
pub fn build(session: &str, events: &[Value]) -> RewindTree {
    let total = events.len() as u64;
    // R2: the markers the kernel's projection drops. Computed first —
    // the scan needs them to know where the *effective* cursor sits (a
    // dropped marker does not move the active path; the kernel pops it
    // and re-projects linear).
    let ignored = ignored_markers(events);
    let mut rounds: Vec<Round> = Vec::new();
    let mut markers: Vec<RewindMarker> = Vec::new();
    let mut tree_boundaries: Vec<Boundary> = Vec::new();
    let mut retract_targets: Vec<String> = Vec::new();
    let mut cursor: Option<usize> = None;
    let mut last_structural: Option<&'static str> = None;

    for (i, ev) in events.iter().enumerate() {
        let seq = (i + 1) as u64;
        let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "user_message" => {
                let idx = rounds.len();
                rounds.push(Round {
                    seq,
                    ts: str_field(ev, "ts"),
                    content: str_field(ev, "content"),
                    id: ev.get("id").and_then(|v| v.as_str()).map(str::to_string),
                    events: 0,
                    retracted: false,
                    parent: cursor,
                    children: Vec::new(),
                });
                if let Some(p) = cursor {
                    rounds[p].children.push(idx);
                }
                cursor = Some(idx);
                last_structural = Some("user_message");
            }
            "rewind" => {
                if let Some(m) = parse_marker(ev, seq) {
                    // Every marker is listed (the log has it), but only
                    // the ones the projection keeps move the cursor.
                    if !ignored.iter().any(|i| i.seq == seq) {
                        let eff = if m.mode == "before" {
                            m.target_seq.saturating_sub(1)
                        } else {
                            m.target_seq
                        };
                        cursor = round_at(&rounds, eff);
                        last_structural = Some("rewind");
                    }
                    markers.push(m);
                }
            }
            "compaction_summary" => {
                if let Some(fk) = ev.get("first_kept_seq").and_then(|v| v.as_u64()) {
                    tree_boundaries.push(Boundary {
                        seq,
                        first_kept_seq: fk,
                        version: ev.get("version").and_then(|v| v.as_u64()).unwrap_or(0),
                    });
                }
            }
            "user_message_retract" => {
                if let Some(t) = ev.get("target").and_then(|v| v.as_str()) {
                    retract_targets.push(t.to_string());
                }
            }
            _ => {}
        }
    }

    // Retracted rounds: a `user_message_retract` names the message id.
    for r in rounds.iter_mut() {
        if let Some(id) = &r.id {
            if retract_targets.iter().any(|t| t == id) {
                r.retracted = true;
            }
        }
    }

    // Folded event count per round: [round.seq, next structural seq).
    let mut structural: Vec<u64> = rounds
        .iter()
        .map(|r| r.seq)
        .chain(markers.iter().map(|m| m.seq))
        .collect();
    structural.sort_unstable();
    for r in rounds.iter_mut() {
        let end = structural
            .iter()
            .find(|&&s| s > r.seq)
            .copied()
            .unwrap_or(total + 1);
        r.events = (r.seq..end)
            .filter(|&s| {
                events
                    .get((s - 1) as usize)
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str())
                    != Some("ext_status")
            })
            .count() as u64;
    }

    // The active path: the final cursor's parent chain.
    let active: Vec<usize> = match cursor {
        Some(mut n) => {
            let mut v = Vec::new();
            loop {
                v.push(n);
                match rounds[n].parent {
                    Some(p) => n = p,
                    None => break,
                }
            }
            v
        }
        None => Vec::new(),
    };

    // R1/R3 (docs/rewind-plugin-plan.md section 11): the kernel's
    // boundary rule and the two guards, computed once for the tree.
    // The *effective* markers drive them — the dropped ones are masked,
    // exactly as the kernel's `mask_active_path` leaves them.
    let ms: Vec<Marker> = markers
        .iter()
        .filter(|m| !ignored.iter().any(|i| i.seq == m.seq))
        .map(|m| Marker {
            seq: m.seq,
            target: m.target_seq,
            before: m.mode == "before",
        })
        .collect();
    let bounds = boundaries(events);
    let restores: Vec<Restore> = rounds
        .iter()
        .map(|r| restore_with(events, r.seq, &ms, &bounds))
        .collect();
    // The tail notice (D-C): the log's last marker is one the kernel
    // dropped — "your rewind did not take effect".
    let last_marker_seq = markers.iter().map(|m| m.seq).max();
    let tail_ignored = ignored
        .iter()
        .find(|i| Some(i.seq) == last_marker_seq)
        .cloned();

    let roots: Vec<RewindNode> = rounds
        .iter()
        .enumerate()
        .filter(|(_, r)| r.parent.is_none())
        .map(|(i, _)| build_node(i, &rounds, &active, cursor, &restores))
        .collect();

    RewindTree {
        session: session.to_string(),
        total_events: total,
        total_rounds: rounds.len() as u64,
        roots,
        rewinds: markers,
        boundaries: tree_boundaries,
        current_seq: cursor.map(|i| rounds[i].seq),
        pending_from: cursor.map(|i| rounds[i].seq),
        settled: last_structural == Some("rewind") && tail_ignored.is_none(),
        ignored,
        tail_ignored,
    }
}

fn build_node(
    idx: usize,
    rounds: &[Round],
    active: &[usize],
    current: Option<usize>,
    restores: &[Restore],
) -> RewindNode {
    let r = &rounds[idx];
    RewindNode {
        seq: r.seq,
        round: (idx + 1) as u64,
        ts: r.ts.clone(),
        summary: summarize(&r.content),
        events: r.events,
        state: if active.contains(&idx) {
            "active"
        } else {
            "abandoned"
        }
        .to_string(),
        current: current == Some(idx),
        retracted: r.retracted,
        restore: restores[idx].clone(),
        children: r
            .children
            .iter()
            .map(|&c| build_node(c, rounds, active, current, restores))
            .collect(),
    }
}

/// The round containing log line `line` (the last `user_message` at or
/// before it), or `None` when the line precedes the first user message.
fn round_at(rounds: &[Round], line: u64) -> Option<usize> {
    if line == 0 {
        return None;
    }
    rounds.iter().rposition(|r| r.seq <= line)
}

/// Parse a `rewind` event with the kernel's validation rules: integer
/// `target_seq >= 1`, a mode of `before`/`on`, and a target strictly earlier
/// than the marker's own line. A malformed marker is ignored.
fn parse_marker(ev: &Value, seq: u64) -> Option<RewindMarker> {
    let target = ev
        .get("target_seq")
        .and_then(|v| v.as_u64())
        .filter(|&t| t >= 1)?;
    if target >= seq {
        return None;
    }
    let mode = match ev.get("mode").and_then(|m| m.as_str()) {
        Some("before") => "before",
        Some("on") => "on",
        _ => return None,
    };
    Some(RewindMarker {
        seq,
        target_seq: target,
        mode: mode.to_string(),
    })
}

fn str_field(ev: &Value, key: &str) -> String {
    ev.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// One-line preview of a user message (whitespace collapsed, ~60 chars).
fn summarize(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 60;
    if flat.chars().count() <= MAX {
        flat
    } else {
        let s: String = flat.chars().take(MAX).collect();
        format!("{s}\u{2026}")
    }
}

// ── the kernel's boundary rule and the two guards (R1/R3) ─────────
//
// docs/rewind-plugin-plan.md section 11. The kernel
// (docs/rewind-fork-design.md section 11) picks the compaction
// boundary from the **active path** and then checks the pair
// invariant; this module mirrors both, purely, so the plugin can say
// what a pick will do before it writes the marker:
//
//   * `restore_at` — what the context at a node is reconstructed from
//     (R3, the annotation);
//   * `rewind_verdict` — whether the kernel would ignore the marker a
//     pick would append (R1, the pre-check);
//   * `ignored_markers` — the markers the current log's projection
//     drops (R2, the post-hoc notice: the kernel warns on the loop's
//     stderr, which the webui cannot read, so the same rule is
//     re-evaluated here — one implementation, server-side).
//
// The port is exact, not approximate: `active_ranges` is asserted
// against the kernel's own fixtures in the tests below, and the
// boundary pick / pair check are the same algorithms. The only
// difference from `bin/assemble` is the follow-queue flag
// (`--inject-follow`): a pending `follow` user message may or may not
// ride the request, but a user message never carries a tool call or a
// result, so the verdict is unaffected.

/// The kernel's boundary (mirror of
/// `rushi_common::compact_math::BoundaryRef`): a plain
/// `compaction_summary`, its log line, and its cutoff. A branch marker
/// (additive `branch_of`) is never a boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryRef {
    pub seq: u64,
    pub first_kept_seq: u64,
    /// The handoff version the marker names (0 on a legacy marker).
    pub version: u64,
}

/// One effective `rewind` marker in the mask recursion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    pub seq: u64,
    pub target: u64,
    pub before: bool,
}

impl Marker {
    /// The effective context boundary: `target`, or `target - 1` in
    /// mode `before` (the kernel's `RewindRef::eff`).
    pub fn eff(&self) -> u64 {
        if self.before {
            self.target.saturating_sub(1)
        } else {
            self.target
        }
    }
}

/// The valid `rewind` markers of the log, in log order. The validation
/// is the kernel's: integer `target_seq >= 1`, a mode of `before`/`on`,
/// a target strictly earlier than the marker's own line.
pub fn markers(events: &[Value]) -> Vec<Marker> {
    let mut out = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        if ev.get("type").and_then(|t| t.as_str()) != Some("rewind") {
            continue;
        }
        let seq = (i + 1) as u64;
        let Some(target) = ev.get("target_seq").and_then(|v| v.as_u64()).filter(|&t| t >= 1) else {
            continue;
        };
        if target >= seq {
            continue;
        }
        let before = match ev.get("mode").and_then(|m| m.as_str()) {
            Some("before") => true,
            Some("on") => false,
            _ => continue,
        };
        out.push(Marker { seq, target, before });
    }
    out
}

/// The active path of the log prefix ending at `end`, as disjoint
/// ascending inclusive `(lo, hi)` seq ranges — a port of
/// `rushi_common::rewind::active_ranges` (the recursion through the
/// rewind chain masks every abandoned branch at every nesting depth).
pub fn active_ranges(end: u64, markers: &[Marker]) -> Vec<(u64, u64)> {
    if end == 0 {
        return Vec::new();
    }
    match markers.iter().rfind(|m| m.seq <= end) {
        None => vec![(1, end)],
        Some(m) => {
            let mut out = active_ranges(m.eff(), markers);
            if end > m.seq {
                out.push((m.seq + 1, end));
            }
            out
        }
    }
}

/// Whether log line `seq` is inside one of the active ranges.
pub fn in_ranges(seq: u64, ranges: &[(u64, u64)]) -> bool {
    ranges.iter().any(|&(lo, hi)| seq >= lo && seq <= hi)
}

/// The plain `compaction_summary` boundaries of the log, in log order
/// (the kernel's `parse_boundary`: a branch marker, a cutoff below 1,
/// or a marker without a summary is not a boundary).
pub fn boundaries(events: &[Value]) -> Vec<BoundaryRef> {
    let mut out = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        if ev.get("type").and_then(|t| t.as_str()) != Some("compaction_summary") {
            continue;
        }
        if ev.get("branch_of").is_some() {
            continue;
        }
        let Some(fk) = ev.get("first_kept_seq").and_then(|v| v.as_u64()).filter(|&f| f >= 1) else {
            continue;
        };
        if ev.get("summary").and_then(|v| v.as_str()).is_none() {
            continue;
        }
        out.push(BoundaryRef {
            seq: (i + 1) as u64,
            first_kept_seq: fk,
            version: ev.get("version").and_then(|v| v.as_u64()).unwrap_or(0),
        });
    }
    out
}

/// The boundary governing the context at `log_len`: the last plain
/// boundary inside `active_ranges(log_len, markers)` whose cutoff fits
/// the log — the kernel's `pick_boundary_on_active_path`. A cutoff
/// beyond the log end is corrupt and skipped.
pub fn boundary_on_active_path(
    bounds: &[BoundaryRef],
    log_len: u64,
    markers: &[Marker],
) -> Option<BoundaryRef> {
    let active = active_ranges(log_len, markers);
    bounds
        .iter()
        .filter(|b| b.first_kept_seq <= log_len.max(1) && in_ranges(b.seq, &active))
        .next_back()
        .cloned()
}

/// The pair-stranding invariant (the kernel's
/// `context_strands_pairs`): the id of the first tool call whose
/// result is missing, or of the first result whose call is missing.
/// `None` when every pair is complete.
fn strands_pair(events: &[&Value]) -> Option<String> {
    let result_ids: HashSet<&str> = events
        .iter()
        .filter(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        .filter_map(|ev| ev.get("id").and_then(|i| i.as_str()))
        .collect();
    let call_ids: HashSet<&str> = events
        .iter()
        .filter(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("assistant_message"))
        .filter_map(|ev| ev.get("tool_calls").and_then(|c| c.as_array()))
        .flatten()
        .filter_map(|c| c.get("id").and_then(|i| i.as_str()))
        .collect();
    for ev in events {
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("assistant_message") => {
                if let Some(calls) = ev.get("tool_calls").and_then(|c| c.as_array()) {
                    for c in calls {
                        if let Some(id) = c.get("id").and_then(|i| i.as_str()) {
                            if !result_ids.contains(id) {
                                return Some(id.to_string());
                            }
                        }
                    }
                }
            }
            Some("tool_result") => {
                if let Some(id) = ev.get("id").and_then(|i| i.as_str()) {
                    if !call_ids.contains(id) {
                        return Some(id.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// R1 — would the kernel ignore the marker this pick would append?
///
/// The kernel never refuses a marker: it appends it and the
/// *projection* drops it when the masked context would strand a tool
/// call without its result (or the reverse, `docs/rewind-fork-
/// design.md` P4) — the branch then re-projects linear and the user's
/// rewind did not happen. This is the same decision, taken before the
/// write, over the log plus the candidate marker.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RewindVerdict {
    /// The marker will take effect.
    Ok,
    /// The kernel's projection will drop it: this tool id would be
    /// stranded.
    StrandsPair { missing: String },
    /// The pick is not a settled target (the producer rule, section 7
    /// of the design doc): a `before` pick must target a user message.
    NotSettled { reason: String },
}

/// The R1 verdict for appending `rewind { target_seq, mode }` to this
/// log now (the candidate sits at `events.len() + 1`).
pub fn rewind_verdict(events: &[Value], target_seq: u64, mode: &str) -> RewindVerdict {
    let ms = markers(events);
    let bounds = boundaries(events);
    verdict_with(events, target_seq, mode, &ms, &bounds)
}

/// The R3 annotation for one log line, over the whole log. The tree
/// calls [`restore_with`] instead (one parse for every node); this
/// entry point is the tested one.
#[allow(dead_code)]
pub fn restore_at(events: &[Value], node_seq: u64) -> Restore {
    let ms = markers(events);
    let bounds = boundaries(events);
    restore_with(events, node_seq, &ms, &bounds)
}

/// [`rewind_verdict`] with the parsed markers/boundaries supplied (the
/// tree computes both once for every node).
fn verdict_with(
    events: &[Value],
    target_seq: u64,
    mode: &str,
    ms: &[Marker],
    bounds: &[BoundaryRef],
) -> RewindVerdict {
    let marker_seq = events.len() as u64 + 1;
    if target_seq < 1 || target_seq >= marker_seq {
        return RewindVerdict::NotSettled {
            reason: "the target must be an earlier log line".to_string(),
        };
    }
    if mode == "before" {
        let ty = events
            .get(target_seq as usize - 1)
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if ty != "user_message" {
            return RewindVerdict::NotSettled {
                reason: "mode `before` restores a user message to the input".to_string(),
            };
        }
    } else if mode != "on" {
        return RewindVerdict::NotSettled {
            reason: format!("unknown mode `{mode}`"),
        };
    }

    let mut all = ms.to_vec();
    all.push(Marker {
        seq: marker_seq,
        target: target_seq,
        before: mode == "before",
    });
    let boundary = boundary_on_active_path(bounds, marker_seq, &all);
    let ranges = active_ranges(marker_seq, &all);
    let first_kept = boundary.as_ref().map(|b| b.first_kept_seq).unwrap_or(1);
    let kept: Vec<&Value> = events
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let seq = *i as u64 + 1;
            seq >= first_kept && in_ranges(seq, &ranges)
        })
        .map(|(_, ev)| ev)
        .collect();
    match strands_pair(&kept) {
        Some(missing) => RewindVerdict::StrandsPair { missing },
        None => RewindVerdict::Ok,
    }
}

/// R3 — how the context at `node_seq` is reconstructed if the user
/// rewinds to it (the kernel's section 11 rule, computed over the
/// current log: the context at the node).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Restore {
    /// No boundary on the active path there: the raw rounds
    /// `1..node_seq`.
    Raw,
    /// The boundary in force at the node: its handoff document plus
    /// the raw events `from_seq..=to_seq`.
    Framed {
        version: u64,
        from_seq: u64,
        to_seq: u64,
    },
    /// The pick would be ignored (R1): the kernel's guard strands this
    /// tool id.
    Unresumable { missing: String },
}

/// [`Restore`] for one node, with the tree's parsed markers and
/// boundaries.
fn restore_with(
    events: &[Value],
    node_seq: u64,
    ms: &[Marker],
    bounds: &[BoundaryRef],
) -> Restore {
    if let RewindVerdict::StrandsPair { missing } = verdict_with(events, node_seq, "on", ms, bounds)
    {
        return Restore::Unresumable { missing };
    }
    let log_len = events.len() as u64 + 1;
    let active = active_ranges(node_seq, ms);
    match bounds
        .iter()
        .filter(|b| b.first_kept_seq <= log_len && in_ranges(b.seq, &active))
        .next_back()
    {
        None => Restore::Raw,
        Some(b) => Restore::Framed {
            version: b.version,
            from_seq: b.first_kept_seq,
            to_seq: node_seq,
        },
    }
}

/// One marker the kernel's projection drops (R2 post-hoc).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IgnoredMarker {
    pub seq: u64,
    pub target_seq: u64,
    pub mode: String,
    /// The tool id that would be stranded.
    pub missing: String,
}

/// The markers the projection of *this* log drops, outermost first — a
/// port of `bin/assemble`'s `mask_active_path` pop loop: while the
/// masked region strands a pair, drop the outermost marker and
/// re-project. Non-empty means a rewind in this log did not take
/// effect (the kernel printed its warning on the loop's stderr; this
/// is the same fact in the tree).
pub fn ignored_markers(events: &[Value]) -> Vec<IgnoredMarker> {
    let end = events.len() as u64;
    let mut ms = markers(events);
    // The projection picks its boundary **once**, before the pops
    // (`bin/assemble`: `boundary_region` then `mask_active_path`).
    let boundary = boundary_on_active_path(&boundaries(events), end, &ms);
    let first_kept = boundary.as_ref().map(|b| b.first_kept_seq).unwrap_or(1);
    let mut ignored = Vec::new();
    loop {
        let ranges = active_ranges(end, &ms);
        let kept: Vec<&Value> = events
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let seq = *i as u64 + 1;
                seq >= first_kept && in_ranges(seq, &ranges)
            })
            .map(|(_, ev)| ev)
            .collect();
        let Some(missing) = strands_pair(&kept) else {
            break;
        };
        if ms.is_empty() {
            break;
        }
        // The outermost marker owns the current gap: pop and
        // re-project.
        let last = ms.pop().expect("the guard above");
        ignored.push(IgnoredMarker {
            seq: last.seq,
            target_seq: last.target,
            mode: if last.before { "before" } else { "on" }.to_string(),
            missing,
        });
    }
    ignored
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── synthetic log builders (1-based line placement is explicit) ──

    fn um(content: &str) -> Value {
        json!({ "v": 1, "type": "user_message", "ts": "t", "content": content })
    }
    fn um_id(content: &str, id: &str) -> Value {
        json!({ "v": 1, "type": "user_message", "ts": "t", "content": content, "id": id })
    }
    fn am() -> Value {
        json!({ "v": 1, "type": "assistant_message", "ts": "t", "content": "x" })
    }
    fn ext() -> Value {
        json!({ "v": 1, "type": "ext_status", "ts": "t", "id": "x", "value": "1" })
    }
    fn rw(target: u64, mode: &str) -> Value {
        json!({ "v": 1, "type": "rewind", "ts": "t", "target_seq": target, "mode": mode })
    }

    /// Flatten (seq, state, current) for every node.
    fn flat(nodes: &[RewindNode], out: &mut Vec<(u64, String, bool)>) {
        for n in nodes {
            out.push((n.seq, n.state.clone(), n.current));
            flat(&n.children, out);
        }
    }

    fn active_seqs(tree: &RewindTree) -> Vec<u64> {
        let mut v = Vec::new();
        flat(&tree.roots, &mut v);
        v.retain(|(_, s, _)| s == "active");
        v.into_iter().map(|(seq, _, _)| seq).collect()
    }

    /// The oracle the projection must agree with
    /// (`rushi/crates/rushi/src/rewind.rs`): the module's port of
    /// `active_ranges`, called with the kernel fixtures' `(seq, target,
    /// before)` tuples.
    fn active_ranges(end: u64, markers: &[(u64, u64, bool)]) -> Vec<(u64, u64)> {
        let ms: Vec<Marker> = markers
            .iter()
            .map(|&(seq, target, before)| Marker {
                seq,
                target,
                before,
            })
            .collect();
        super::active_ranges(end, &ms)
    }

    fn in_ranges(seq: u64, ranges: &[(u64, u64)]) -> bool {
        super::in_ranges(seq, ranges)
    }

    /// The oracle assertion: every round's `state` equals "active" iff its
    /// line is inside the kernel's `active_ranges(total)`.
    fn assert_matches_kernel(tree: &RewindTree, markers: &[(u64, u64, bool)]) {
        let ranges = active_ranges(tree.total_events, markers);
        let mut nodes = Vec::new();
        flat(&tree.roots, &mut nodes);
        assert_eq!(nodes.len() as u64, tree.total_rounds);
        for (seq, state, _) in nodes {
            let want = in_ranges(seq, &ranges);
            assert_eq!(
                state == "active",
                want,
                "round at seq {seq}: state {state:?}, active_ranges {ranges:?}"
            );
        }
    }

    // ── fixtures mirroring the kernel's rewind.rs tests ───────────────

    #[test]
    fn no_rewinds_is_one_straight_line() {
        let events = vec![um("u1"), am(), um("u2"), am(), um("u3")];
        let tree = build("s", &events);
        let mut nodes = Vec::new();
        flat(&tree.roots, &mut nodes);
        assert_eq!(tree.total_rounds, 3);
        assert!(nodes.iter().all(|(_, s, _)| s == "active"));
        assert_eq!(tree.current_seq, Some(5));
        assert!(nodes.iter().any(|(seq, _, cur)| *seq == 5 && *cur));
        assert!(!tree.settled);
        // one root, a single chain
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].children.len(), 1);
        assert_matches_kernel(&tree, &[]);
    }

    #[test]
    fn nested_forks_mask_the_abandoned_branch() {
        // Kernel fixture: 1..3 = A, rewind(4→3) forks B (5..6),
        // rewind(7→3) forks A' (8..9), rewind(10→9) continues A'.
        // active_ranges(10) = [(1,3), (8,9)].
        let events = vec![
            um("a1"),
            am(),
            um("a2"),
            rw(3, "on"),
            um("b1"),
            um("b2"),
            rw(3, "on"),
            um("c1"),
            um("c2"),
            rw(9, "on"),
        ];
        let tree = build("s", &events);
        assert_matches_kernel(
            &tree,
            &[(4, 3, false), (7, 3, false), (10, 9, false)],
        );
        let active = active_seqs(&tree);
        assert_eq!(active, vec![1, 3, 8, 9]);
        assert_eq!(tree.current_seq, Some(9), "settled at the target round");
        assert!(tree.settled);
        // The fork: a2 (3) has both b1 (5) and c1 (8) as children.
        let a2 = tree.roots[0].children[0].clone();
        assert_eq!(a2.seq, 3);
        let kids: Vec<u64> = a2.children.iter().map(|n| n.seq).collect();
        assert_eq!(kids, vec![5, 8]);
        assert_eq!(
            a2.children.iter().map(|n| n.state.as_str()).collect::<Vec<_>>(),
            vec!["abandoned", "active"]
        );
    }

    #[test]
    fn reentering_a_branch_rebuilds_its_path() {
        // Kernel fixture: A→B→A′ then rewind(10→6) re-enters B at its tail.
        // active_ranges(12) = [(1,3), (5,6), (11,12)].
        let events = vec![
            um("a1"),
            am(),
            um("a2"),
            rw(3, "on"),
            um("b1"),
            um("b2"),
            rw(3, "on"),
            um("c1"),
            um("c2"),
            rw(6, "on"),
            um("d1"),
            um("d2"),
        ];
        let tree = build("s", &events);
        assert_matches_kernel(
            &tree,
            &[(4, 3, false), (7, 3, false), (10, 6, false)],
        );
        assert_eq!(active_seqs(&tree), vec![1, 3, 5, 6, 11, 12]);
        assert_eq!(tree.current_seq, Some(12));
        // Re-entry: b2 (6) gains d1 (11) as a child.
        let b2 = tree.roots[0].children[0].children[0].children[0].clone();
        assert_eq!(b2.seq, 6);
        assert_eq!(b2.children.iter().map(|n| n.seq).collect::<Vec<_>>(), vec![11]);
        assert_eq!(b2.children[0].state, "active");
    }

    #[test]
    fn deep_chains_follow_the_nested_targets() {
        // Kernel fixture: rewind(4→3), rewind(8→5), rewind(11→8).
        // active_ranges(13) = [(1,3), (5,5), (12,13)].
        let events = vec![
            um("a"),
            am(),
            um("b"),
            rw(3, "on"),
            um("c"),
            am(),
            am(),
            rw(5, "on"),
            um("d"),
            am(),
            rw(8, "on"),
            um("e"),
            um("f"),
        ];
        let tree = build("s", &events);
        assert_matches_kernel(
            &tree,
            &[(4, 3, false), (8, 5, false), (11, 8, false)],
        );
        assert_eq!(active_seqs(&tree), vec![1, 3, 5, 12, 13]);
    }

    #[test]
    fn before_mode_excludes_the_target() {
        // Kernel fixture: rewind(4→3, before) → eff 2; active(5) = [(1,2),(5,5)].
        let events = vec![um("u1"), am(), um("u2"), rw(3, "before"), um("u3")];
        let tree = build("s", &events);
        assert_matches_kernel(&tree, &[(4, 3, true)]);
        assert_eq!(active_seqs(&tree), vec![1, 5]);
        assert_eq!(tree.current_seq, Some(5));
        // u2 (seq 3) is the abandoned sibling under u1 (seq 1).
        let u1 = &tree.roots[0];
        assert_eq!(u1.seq, 1);
        assert_eq!(u1.children.iter().map(|n| n.seq).collect::<Vec<_>>(), vec![3, 5]);
    }

    #[test]
    fn rewind_at_log_end_has_no_continuation() {
        // Kernel fixture: active_ranges(5) = [(1,2)].
        let events = vec![um("u1"), am(), um("u2"), am(), rw(2, "on")];
        let tree = build("s", &events);
        assert_matches_kernel(&tree, &[(5, 2, false)]);
        assert_eq!(active_seqs(&tree), vec![1]);
        assert_eq!(tree.current_seq, Some(1));
        assert!(tree.settled);
    }

    #[test]
    fn malformed_markers_are_ignored() {
        let events = vec![
            um("u1"),
            json!({ "v": 1, "type": "rewind", "ts": "t", "target_seq": 0, "mode": "on" }),
            json!({ "v": 1, "type": "rewind", "ts": "t", "target_seq": 1 }),
            json!({ "v": 1, "type": "rewind", "ts": "t", "target_seq": 9, "mode": "on" }),
            um("u2"),
        ];
        let tree = build("s", &events);
        assert!(tree.rewinds.is_empty());
        assert_eq!(active_seqs(&tree), vec![1, 5]);
        assert_eq!(tree.current_seq, Some(5));
    }

    #[test]
    fn round_metadata_and_event_folding() {
        let events = vec![
            um("hello   world\nsecond line"),
            am(),
            ext(),
            am(),
            um("next"),
        ];
        let tree = build("s", &events);
        assert_eq!(tree.roots[0].round, 1);
        assert_eq!(tree.roots[0].summary, "hello world second line");
        // [1..5): um, am, ext, am → ext_status is not counted.
        assert_eq!(tree.roots[0].events, 3);
        assert_eq!(tree.roots[0].children[0].round, 2);
    }

    #[test]
    fn retract_and_boundary_annotations() {
        let events = vec![
            um_id("old", "m1"),
            am(),
            json!({ "v": 1, "type": "user_message_retract", "ts": "t", "target": "m1", "reason": "user_edit" }),
            um("new"),
            json!({ "v": 1, "type": "compaction_summary", "ts": "t", "summary": "s", "version": 1, "parent_version": 0, "diverge_seq": 0, "first_kept_seq": 4 }),
        ];
        let tree = build("s", &events);
        assert!(tree.roots[0].retracted);
        assert_eq!(tree.roots[0].summary, "old");
        assert_eq!(tree.boundaries.len(), 1);
        assert_eq!(tree.boundaries[0].first_kept_seq, 4);
    }

    #[test]
    fn empty_log_yields_an_empty_tree() {
        let tree = build("s", &[]);
        assert_eq!(tree.total_rounds, 0);
        assert!(tree.roots.is_empty());
        assert_eq!(tree.current_seq, None);
        assert!(!tree.settled);
    }

    #[test]
    fn rewind_target_before_the_first_round_makes_a_second_root() {
        let events = vec![ext(), um("u1"), am(), rw(1, "on"), um("u2")];
        let tree = build("s", &events);
        assert_matches_kernel(&tree, &[(4, 1, false)]);
        assert_eq!(tree.roots.len(), 2, "no round at line 1 → a root-level fork");
        assert_eq!(active_seqs(&tree), vec![5]);
        assert_eq!(tree.current_seq, Some(5));
    }

    // ── R1/R3: the boundary rule and the two guards (plan 11) ───────

    fn boundary(fk: u64, version: u64) -> Value {
        json!({
            "v": 1, "type": "compaction_summary", "ts": "t",
            "summary": format!("summary {version}"), "first_kept_seq": fk,
            "version": version, "parent_version": 0, "diverge_seq": 0,
            "reason": "threshold", "tokens_before": 0
        })
    }

    fn branch(fk: u64, of: u64) -> Value {
        json!({
            "v": 1, "type": "compaction_summary", "ts": "t",
            "summary": "branch", "first_kept_seq": fk, "branch_of": of,
            "version": 1, "parent_version": 0, "diverge_seq": of,
            "reason": "threshold", "tokens_before": 0
        })
    }

    fn call(id: &str) -> Value {
        json!({
            "v": 1, "type": "assistant_message", "ts": "t", "content": "",
            "tool_calls": [{ "id": id, "name": "bash", "arguments": { "command": "x" } }],
            "stop_reason": "tool_calls"
        })
    }

    fn res(id: &str) -> Value {
        json!({ "v": 1, "type": "tool_result", "ts": "t", "id": id,
                "value": { "text": "out" }, "is_error": false })
    }

    /// The session-`rewind` shape: v1 (seq 5, cutoff 4) is in force at
    /// the rewind target (seq 6); v2 (seq 12, cutoff 11) landed inside
    /// the span the rewind at 13 abandons. The boundary is v1 — the
    /// kernel's section 11 rule, mirrored.
    #[test]
    fn boundary_on_active_path_ignores_an_abandoned_boundary() {
        let events = vec![
            um("task one"),  // 1
            call("g1"),      // 2
            res("g1"),       // 3
            am(),            // 4
            boundary(4, 1),  // 5
            um("task two"),  // 6
            call("g2"),      // 7
            res("g2"),       // 8
            am(),            // 9
            call("g3"),      // 10
            res("g3"),       // 11
            boundary(11, 2), // 12
            rw(6, "on"),     // 13
        ];
        let ms = markers(&events);
        let bounds = boundaries(&events);
        assert_eq!(bounds.len(), 2, "two plain boundaries");
        let b = boundary_on_active_path(&bounds, 13, &ms).expect("a boundary");
        assert_eq!((b.seq, b.version, b.first_kept_seq), (5, 1, 4));
        // A branch marker is never a boundary.
        let with_branch = vec![um("a"), boundary(2, 1), branch(1, 3)];
        assert_eq!(boundaries(&with_branch).len(), 1);

        // R3: the node at seq 6 resumes from v1's framing; the round
        // before the boundary is raw.
        let ms = markers(&events);
        let bounds = boundaries(&events);
        assert_eq!(
            restore_with(&events, 6, &ms, &bounds),
            Restore::Framed { version: 1, from_seq: 4, to_seq: 6 }
        );
        assert_eq!(restore_with(&events, 1, &ms, &bounds), Restore::Raw);
        // And the tree carries it.
        let tree = build("s", &events);
        let mut nodes = Vec::new();
        flat(&tree.roots, &mut nodes);
        assert_eq!(nodes.len(), 2);
        let task_two = flat_restore(&tree, 6);
        assert_eq!(
            task_two,
            Restore::Framed { version: 1, from_seq: 4, to_seq: 6 }
        );
    }

    /// Every node's `restore` in the tree: walk and find the seq.
    fn flat_restore(tree: &RewindTree, seq: u64) -> Restore {
        fn find(nodes: &[RewindNode], seq: u64) -> Option<Restore> {
            for n in nodes {
                if n.seq == seq {
                    return Some(n.restore.clone());
                }
                if let Some(r) = find(&n.children, seq) {
                    return Some(r);
                }
            }
            None
        }
        find(&tree.roots, seq).expect("the node")
    }

    /// R1: a pick mid-step strands the tool call the log left open for
    /// it — the kernel's P4 guard would ignore the marker.
    #[test]
    fn verdict_flags_a_stranded_pair() {
        // The kernel's `sE` fixture: a steer message lands between the
        // call and its result; the rewind at 5 targets the steer line.
        let events = vec![um("task"), call("c1"), um("steer"), res("c1")];
        assert_eq!(
            rewind_verdict(&events, 3, "on"),
            RewindVerdict::StrandsPair { missing: "c1".to_string() }
        );
        // The settled pick (the round's first message) is fine.
        assert_eq!(rewind_verdict(&events, 1, "on"), RewindVerdict::Ok);
        // Mode `before` on the steer line is the same strand: the
        // target is excluded, so the masked result strands the call
        // (the kernel's `sE` marker is exactly this pick).
        assert_eq!(
            rewind_verdict(&events, 3, "before"),
            RewindVerdict::StrandsPair { missing: "c1".to_string() }
        );
        // `before` on the next round's message is fine: the pair is
        // complete below it.
        let settled = vec![um("a"), call("c1"), res("c1"), um("b")];
        assert_eq!(rewind_verdict(&settled, 4, "before"), RewindVerdict::Ok);
        // The producer rule: `before` on a non-user line.
        assert!(matches!(
            rewind_verdict(&events, 2, "before"),
            RewindVerdict::NotSettled { .. }
        ));
        // Out-of-range targets never pass the shape check.
        assert!(matches!(
            rewind_verdict(&events, 5, "on"),
            RewindVerdict::NotSettled { .. }
        ));
    }

    /// R2 post-hoc: the marker the projection drops is reported, and
    /// the tree flags the tail one (the "your rewind did not take
    /// effect" notice).
    #[test]
    fn ignored_markers_report_the_dropped_marker() {
        let events = vec![um("task"), call("c1"), um("steer"), res("c1"), rw(3, "before")];
        let ignored = ignored_markers(&events);
        assert_eq!(ignored.len(), 1);
        assert_eq!(
            (ignored[0].seq, ignored[0].target_seq, ignored[0].mode.as_str(),
             ignored[0].missing.as_str()),
            (5, 3, "before", "c1")
        );
        let tree = build("s", &events);
        assert_eq!(tree.tail_ignored.as_ref().map(|i| i.seq), Some(5));
        // A linear log reports nothing.
        let clean = vec![um("a"), call("c1"), res("c1")];
        assert!(ignored_markers(&clean).is_empty());
        assert!(build("s", &clean).tail_ignored.is_none());
    }

    /// A dropped marker does not move the cursor: the kernel pops it and
    /// re-projects linear, so the tree's active path, its states and
    /// every annotation come from the markers that survive.
    #[test]
    fn a_dropped_marker_does_not_move_the_tree_cursor() {
        let events = vec![
            um("round one"),  // 1
            am(),             // 2
            boundary(1, 1),   // 3
            um("round two"),  // 4
            call("c1"),       // 5
            um("steer"),      // 6
            res("c1"),        // 7
            um("round four"), // 8
            am(),             // 9
            boundary(8, 2),   // 10
            rw(4, "on"),      // 11
            am(),             // 12
            um("round five"), // 13
            am(),             // 14
            rw(6, "on"),      // 15 — the kernel ignores this one
        ];
        assert_eq!(
            ignored_markers(&events),
            vec![IgnoredMarker {
                seq: 15,
                target_seq: 6,
                mode: "on".to_string(),
                missing: "c1".to_string(),
            }]
        );
        let tree = build("s", &events);
        assert_eq!(tree.current_seq, Some(13), "the dropped marker is not the cursor");
        assert_eq!(tree.pending_from, Some(13));
        assert!(!tree.settled, "the tail marker did not take effect");
        assert_eq!(tree.tail_ignored.as_ref().map(|i| i.seq), Some(15));
        let mut nodes = Vec::new();
        flat(&tree.roots, &mut nodes);
        let states: Vec<(u64, String)> =
            nodes.iter().map(|(seq, state, _)| (*seq, state.clone())).collect();
        assert_eq!(
            states,
            vec![
                (1, "active".to_string()),
                (4, "active".to_string()),
                (6, "abandoned".to_string()),
                (8, "abandoned".to_string()),
                (13, "active".to_string())
            ],
            "the active path is the one the kernel projects"
        );
        assert_eq!(
            flat_restore(&tree, 13),
            Restore::Framed {
                version: 1,
                from_seq: 1,
                to_seq: 13
            }
        );
        assert_eq!(
            flat_restore(&tree, 6),
            Restore::Unresumable {
                missing: "c1".to_string()
            }
        );
    }

    /// R3 on a mid-step pick: the node is annotated `Unresumable`, so
    /// the client can refuse the pick before the marker is written.
    #[test]
    fn a_mid_step_node_is_unresumable() {
        let events = vec![um("task"), call("c1"), um("steer"), res("c1")];
        assert_eq!(
            restore_at(&events, 3),
            Restore::Unresumable { missing: "c1".to_string() }
        );
        assert_eq!(restore_at(&events, 1), Restore::Raw);
        let tree = build("s", &events);
        assert_eq!(
            flat_restore(&tree, 3),
            Restore::Unresumable { missing: "c1".to_string() }
        );
    }
}
