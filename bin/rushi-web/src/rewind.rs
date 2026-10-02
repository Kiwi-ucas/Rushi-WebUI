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
    pub children: Vec<RewindNode>,
}

/// One parsed `rewind` marker, at its 1-based log line.
#[derive(Debug, Clone, Serialize)]
pub struct RewindMarker {
    pub seq: u64,
    pub target_seq: u64,
    pub mode: String,
}

/// One compaction boundary: a rewind targeting below `first_kept_seq`
/// degrades to the boundary (kernel rule I4).
#[derive(Debug, Clone, Serialize)]
pub struct Boundary {
    pub seq: u64,
    pub first_kept_seq: u64,
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
    let mut rounds: Vec<Round> = Vec::new();
    let mut markers: Vec<RewindMarker> = Vec::new();
    let mut boundaries: Vec<Boundary> = Vec::new();
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
                    let eff = if m.mode == "before" {
                        m.target_seq.saturating_sub(1)
                    } else {
                        m.target_seq
                    };
                    cursor = round_at(&rounds, eff);
                    markers.push(m);
                    last_structural = Some("rewind");
                }
            }
            "compaction_summary" => {
                if let Some(fk) = ev.get("first_kept_seq").and_then(|v| v.as_u64()) {
                    boundaries.push(Boundary {
                        seq,
                        first_kept_seq: fk,
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

    let roots: Vec<RewindNode> = rounds
        .iter()
        .enumerate()
        .filter(|(_, r)| r.parent.is_none())
        .map(|(i, _)| build_node(i, &rounds, &active, cursor))
        .collect();

    RewindTree {
        session: session.to_string(),
        total_events: total,
        total_rounds: rounds.len() as u64,
        roots,
        rewinds: markers,
        boundaries,
        current_seq: cursor.map(|i| rounds[i].seq),
        pending_from: cursor.map(|i| rounds[i].seq),
        settled: last_structural == Some("rewind"),
    }
}

fn build_node(
    idx: usize,
    rounds: &[Round],
    active: &[usize],
    current: Option<usize>,
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
        children: r
            .children
            .iter()
            .map(|&c| build_node(c, rounds, active, current))
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

    /// A ported reference of the kernel's `active_ranges`
    /// (`rushi/crates/rushi/src/rewind.rs`) — the oracle the projection must
    /// agree with. Markers are `(seq, target, before)`.
    fn active_ranges(end: u64, markers: &[(u64, u64, bool)]) -> Vec<(u64, u64)> {
        if end == 0 {
            return Vec::new();
        }
        match markers.iter().rfind(|m| m.0 <= end) {
            None => vec![(1, end)],
            Some(&(s, t, before)) => {
                let eff = if before { t.saturating_sub(1) } else { t };
                let mut out = active_ranges(eff, markers);
                if end > s {
                    out.push((s + 1, end));
                }
                out
            }
        }
    }

    fn in_ranges(seq: u64, ranges: &[(u64, u64)]) -> bool {
        ranges.iter().any(|&(lo, hi)| seq >= lo && seq <= hi)
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
}
