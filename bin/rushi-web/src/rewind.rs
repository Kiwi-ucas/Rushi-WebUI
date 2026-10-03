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

use std::collections::{HashMap, HashSet};

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
    /// Style B's geometry (section 10) — a projection of the same tree;
    /// style A ignores it.
    pub flow: Flow,
}

/// The geometric projection behind the History view's **second style**
/// ("flow": `docs/rewind-plugin-plan.md` section 10).
///
/// The conversation's **longest chain is the straight main line** (`lane`
/// 0, one column per round — the horizontal `. - . - .` picture). Every
/// other chain segment forks off that line into a lane above (`-1`, `-2`,
/// …) or below (`+1`, `+2`, …) of the column it forked at, and keeps it.
///
/// Logical units only — no pixels: the client multiplies `x` and `lane`
/// by its own constants. The geometry lives here, and not in the client,
/// because the WASM crate cannot be unit-tested (section 10.1.1).
#[derive(Debug, Clone, Serialize)]
pub struct Flow {
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<FlowEdge>,
    /// One entry per chain segment, the main line included (`lane` 0), so
    /// the client can draw — and turn — one ribbon per segment.
    pub branches: Vec<FlowBranch>,
    /// The grid's width in columns (`max x + 1`).
    pub cols: u64,
    /// The grid's half-height (`max |lane|`); `0` for a straight line.
    pub lanes: i32,
    /// The main line's length in nodes.
    pub main_len: u64,
    /// **v0.5.68**: the orbital view's ring parameters (the angular step, the
    /// visible arc, the fin count).
    pub orbit: Orbit,
}

/// One node's place in the flow picture, with everything the detail panel
/// shows for it (the same values the recursive [`RewindNode`] carries).
#[derive(Debug, Clone, Serialize)]
pub struct FlowNode {
    pub seq: u64,
    pub round: u64,
    /// The column: one step per round down the tree.
    pub x: u64,
    /// `0` on the main line, `-1`/`+1`/`-2`/`+2`… forking up/down.
    pub lane: i32,
    /// Sits on the longest chain, i.e. the straight line.
    pub main: bool,
    pub state: String,
    pub current: bool,
    pub retracted: bool,
    pub events: u64,
    pub ts: String,
    pub summary: String,
    pub restore: Restore,
}

/// One parent→child connector. `lane`/`main` describe the child, which is
/// what the client needs to draw a straight `. - .` link or a fork's elbow.
#[derive(Debug, Clone, Serialize)]
pub struct FlowEdge {
    pub from: u64,
    pub to: u64,
    pub lane: i32,
    pub main: bool,
}

/// A chain segment: the nodes that share one lane from `from_x` to `to_x`
/// inclusive (the columns it occupies, so no two segments of a lane ever
/// overlap — a test asserts it).
#[derive(Debug, Clone, Serialize)]
pub struct FlowBranch {
    /// The seq of the segment's first node.
    pub root: u64,
    pub lane: i32,
    /// The lane of the segment it forked off (`0` for the main line).
    pub parent_lane: i32,
    pub from_x: u64,
    pub to_x: u64,
    /// **v0.5.68 (the orbital view)**: this segment's index among the
    /// *fins* — the branches that are not the main line — or `None` on the
    /// main line. It is the ring's slot order: the client turns it into an
    /// angle (`(fin - align) * step`), so the server owns the ordering (and
    /// its test) while the client owns the pixels.
    pub fin: Option<u32>,
    /// **v0.5.68**: the column the segment hinges on — its **parent's**
    /// column, i.e. where the branch leaves the trunk. This is the fin's
    /// left edge and the origin of its `rotateX`, so the fin is a radial
    /// drawing of the same tree rather than a floating card.
    pub hinge_x: u64,
    /// **v0.5.68**: the seqs of the rounds in this segment, in walk order.
    /// The client groups the flat `nodes` list into fins with this (a nested
    /// fork shares its parent's lane, so the lane alone cannot group them).
    pub seqs: Vec<u64>,
}

/// **v0.5.68**: the orbital view's logical geometry — the numbers the client
/// cannot invent and the unit tests can pin. The *pixels* (`--r`, the panel
/// size) stay client-side, because only the DOM knows them (see the plan's
/// §12.3: logical geometry server-side, layout client-side).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Orbit {
    /// Degrees between neighbouring fins on the ring.
    pub step_deg: i32,
    /// Half-width of the **visible arc**: a fin whose effective angle is
    /// beyond it queues at the arc's end (D-orb-2, user decision — a bounded
    /// arc with the extra branches parked at the ends, not a full ring).
    pub arc_deg: i32,
    /// How many fins the scene has (`branches` with `lane != 0`).
    pub fins: u32,
}

/// One round in full, for the flow view's top panel (B2). The tree and the
/// flow keep only the 60-char [`summarize`]d preview of every round (a
/// session's whole user-message text is 4-47 KB, small — but the panel needs
/// exactly one round at a time, so it is fetched on selection).
#[derive(Debug, Clone, Serialize)]
pub struct NodeDetail {
    pub seq: u64,
    pub round: u64,
    pub ts: String,
    /// The user message, verbatim (never summarized).
    pub text: String,
    /// The agent's work folded into this round.
    pub events: u64,
    /// `"active"` or `"abandoned"` (the round's place on the active path).
    pub state: String,
    /// The "you are here" round.
    pub current: bool,
    pub retracted: bool,
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
    build_live(session, events, false)
}

/// [`build`] for a *live* session (v0.5.69, round-3 defect 1).
///
/// `live` is whether the session's loop is alive right now. While it
/// is, a tool call whose result is not in the log yet is **pending**,
/// not stranded: the projection a user sees must not collapse because
/// the agent happens to be inside a tool call at that instant (the
/// kernel never assembles the context in that state — see
/// [`strands_pair`]). `live = false` is the exact kernel rule.
pub fn build_live(session: &str, events: &[Value], live: bool) -> RewindTree {
    let total = events.len() as u64;
    // R2: the markers the kernel's projection drops. Computed first —
    // the scan needs them to know where the *effective* cursor sits (a
    // dropped marker does not move the active path; the kernel pops it
    // and re-projects linear).
    let pending = pending_ids(events, live);
    let ignored = ignored_markers_with(events, pending.as_ref());
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
        .map(|r| restore_with(events, r.seq, &ms, &bounds, pending.as_ref()))
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
        flow: flow(&rounds, &active, cursor, &restores),
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

/// The full detail of the round whose `user_message` sits at 1-based log
/// line `seq` — `None` when that line is not a round's message (an
/// assistant message, the middle of a round, past the end of the log…),
/// which the route answers with 404.
pub fn node_detail(events: &[Value], seq: u64) -> Option<NodeDetail> {
    let idx = seq.checked_sub(1)? as usize;
    let ev = events.get(idx)?;
    if ev.get("type").and_then(|t| t.as_str()) != Some("user_message") {
        return None;
    }
    // The meta comes from the same projection the tree is built from, so
    // the panel and the node it was opened for cannot disagree.
    let tree = build("", events);
    let n = tree.flow.nodes.iter().find(|n| n.seq == seq)?;
    Some(NodeDetail {
        seq,
        round: n.round,
        ts: n.ts.clone(),
        text: str_field(ev, "content"),
        events: n.events,
        state: n.state.clone(),
        current: n.current,
        retracted: n.retracted,
    })
}

// ── the flow projection (Style B) ─────────────────────────────────

/// Build the flow picture: the main line, the lanes, the connectors.
/// Pure — every rule is covered by the tests at the end of this module.
/// **v0.5.68 (D-orb-2/8, user decisions)**: the angular step between
/// neighbouring fins. With a 30° step the visible arc below holds five fins.
const ORBIT_STEP_DEG: i32 = 30;
/// Half the visible arc. Fins whose effective angle leaves `[-60, +60]` are
/// *parked*: clamped to the arc's end and stacked behind it, so a session
/// with many forks queues instead of crowding (and the scroll brings each
/// one round).
const ORBIT_ARC_DEG: i32 = 60;

fn flow(
    rounds: &[Round],
    active: &[usize],
    current: Option<usize>,
    restores: &[Restore],
) -> Flow {
    let n = rounds.len();
    if n == 0 {
        return Flow {
            nodes: Vec::new(),
            edges: Vec::new(),
            branches: Vec::new(),
            cols: 0,
            lanes: 0,
            main_len: 0,
            orbit: Orbit {
                step_deg: ORBIT_STEP_DEG,
                arc_deg: ORBIT_ARC_DEG,
                fins: 0,
            },
        };
    }
    let active_set: HashSet<usize> = active.iter().copied().collect();

    // 1. `height[i]` = the node count of the longest chain starting at `i`
    //    (a child always has a higher index: a round is created after its
    //    parent, so one reverse pass suffices).
    let mut height = vec![1u64; n];
    let mut holds_current = vec![false; n];
    for i in (0..n).rev() {
        height[i] = 1 + rounds[i]
            .children
            .iter()
            .map(|&c| height[c])
            .max()
            .unwrap_or(0);
        holds_current[i] =
            current == Some(i) || rounds[i].children.iter().any(|&c| holds_current[c]);
    }

    // 2. The main line: the longest root-to-leaf chain over the whole
    //    tree; ties go to the chain holding the current round, then to the
    //    leftmost one (section 10.2 — the user asked for exactly this).
    //    Note the consequence: an abandoned branch can *be* the straight
    //    line, and the current round then sits on a fork.
    let roots: Vec<usize> = (0..n).filter(|&i| rounds[i].parent.is_none()).collect();
    let rank = |i: usize| (height[i], holds_current[i], std::cmp::Reverse(i));
    let mut main_set = vec![false; n];
    let mut main_chain: Vec<usize> = Vec::new();
    if let Some(mut cur) = roots.iter().copied().max_by_key(|&i| rank(i)) {
        loop {
            main_set[cur] = true;
            main_chain.push(cur);
            match rounds[cur]
                .children
                .iter()
                .copied()
                .max_by_key(|&c| rank(c))
            {
                Some(c) => cur = c,
                None => break,
            }
        }
    }

    // 3. `x`: one column per step down, so a chain advances one column per
    //    round and a fork runs parallel to the line it left. A second root
    //    (a rewind that reached before the first round) is laid out after
    //    the previous tree.
    let mut x = vec![0u64; n];
    let mut base = 0u64;
    for &r in &roots {
        base = layout_x(rounds, r, base, &mut x) + 1;
    }

    // 4. `lane`: 0 on the main line; a segment that forks off takes the
    //    first free lane of the alternating sequence, biased away from the
    //    line, and every node of that segment keeps it.
    let primary = primary_children(rounds, &main_set, &height, &holds_current);
    let mut extent = vec![0u64; n];
    for i in (0..n).rev() {
        extent[i] = match primary[i] {
            Some(c) => x[i].max(extent[c]),
            None => x[i],
        };
    }
    let mut lane = vec![0i32; n];
    let mut taken: HashMap<i32, Vec<(u64, u64)>> = HashMap::new();
    let mut branches: Vec<FlowBranch> = Vec::new();
    for &r in &roots {
        let (from_x, to_x) = (x[r], extent[r]);
        taken.entry(0).or_default().push((from_x, to_x));
        branches.push(FlowBranch {
            root: rounds[r].seq,
            lane: 0,
            parent_lane: 0,
            from_x,
            to_x,
            fin: None,
            // the trunk itself hinges on its own first column
            hinge_x: from_x,
            seqs: Vec::new(),
        });
        let cur = branches.len() - 1;
        place(
            rounds, &x, &extent, &primary, &mut lane, &mut taken, &mut branches, r, 0, 0, cur,
        );
    }
    // The fins: every branch that is not the main line, in branch order.
    // The ring's slot order is this order, so it is stable across reloads.
    let mut fins = 0u32;
    for b in branches.iter_mut() {
        if b.lane != 0 {
            b.fin = Some(fins);
            fins += 1;
        }
    }

    // 5. The flat list (round order = log order) and the connectors.
    let mut nodes = Vec::with_capacity(n);
    let mut edges = Vec::new();
    for i in 0..n {
        nodes.push(FlowNode {
            seq: rounds[i].seq,
            round: (i + 1) as u64,
            x: x[i],
            lane: lane[i],
            main: main_set[i],
            state: if active_set.contains(&i) {
                "active"
            } else {
                "abandoned"
            }
            .to_string(),
            current: current == Some(i),
            retracted: rounds[i].retracted,
            events: rounds[i].events,
            ts: rounds[i].ts.clone(),
            summary: summarize(&rounds[i].content),
            restore: restores[i].clone(),
        });
        for &c in &rounds[i].children {
            edges.push(FlowEdge {
                from: rounds[i].seq,
                to: rounds[c].seq,
                lane: lane[c],
                main: main_set[c],
            });
        }
    }

    Flow {
        cols: x.iter().copied().max().unwrap_or(0) + 1,
        lanes: lane.iter().map(|l| l.abs()).max().unwrap_or(0),
        main_len: main_chain.len() as u64,
        orbit: Orbit {
            step_deg: ORBIT_STEP_DEG,
            arc_deg: ORBIT_ARC_DEG,
            fins,
        },
        nodes,
        edges,
        branches,
    }
}

/// Place `i` and its subtree at `base`, one column per step down; returns
/// the largest column the subtree uses.
fn layout_x(rounds: &[Round], i: usize, base: u64, x: &mut [u64]) -> u64 {
    x[i] = base;
    let mut mx = base;
    for &c in &rounds[i].children {
        mx = mx.max(layout_x(rounds, c, base + 1, x));
    }
    mx
}

/// Which child keeps its parent's lane: on the main line, the main-line
/// child; elsewhere the child that holds the current round, then the
/// tallest, then the earliest. Every other child starts a new segment.
fn primary_children(
    rounds: &[Round],
    main_set: &[bool],
    height: &[u64],
    holds_current: &[bool],
) -> Vec<Option<usize>> {
    (0..rounds.len())
        .map(|i| {
            if main_set[i] {
                return rounds[i].children.iter().copied().find(|&c| main_set[c]);
            }
            rounds[i]
                .children
                .iter()
                .copied()
                .max_by_key(|&c| (holds_current[c], height[c], std::cmp::Reverse(c)))
        })
        .collect()
}

/// Walk one chain segment: `i` stays in `l`, and every other child forks
/// into its own lane — registered beforehand, so two segments never cover
/// the same column in the same lane.
#[allow(clippy::too_many_arguments)]
fn place(
    rounds: &[Round],
    x: &[u64],
    extent: &[u64],
    primary: &[Option<usize>],
    lane: &mut [i32],
    taken: &mut HashMap<i32, Vec<(u64, u64)>>,
    branches: &mut Vec<FlowBranch>,
    i: usize,
    l: i32,
    parent_lane: i32,
    cur: usize,
) {
    lane[i] = l;
    branches[cur].seqs.push(rounds[i].seq);
    for &c in &rounds[i].children {
        if Some(c) == primary[i] {
            place(rounds, x, extent, primary, lane, taken, branches, c, l, parent_lane, cur);
            continue;
        }
        let (from_x, to_x) = (x[c], extent[c]);
        let nl = pick_lane(taken, l, from_x, to_x);
        taken.entry(nl).or_default().push((from_x, to_x));
        branches.push(FlowBranch {
            root: rounds[c].seq,
            lane: nl,
            parent_lane: l,
            from_x,
            to_x,
            fin: None,          // assigned after the walk (the ring's order)
            hinge_x: x[i],      // where it leaves the trunk: its parent's column
            seqs: Vec::new(),
        });
        let next = branches.len() - 1;
        place(rounds, x, extent, primary, lane, taken, branches, c, nl, l, next);
    }
}

/// The first free lane for a segment covering the columns `from_x..=to_x`:
/// alternate the sides outward from `parent_lane`, biased *away* from the
/// main line (`|lane|` grows), so a fork inside an upper branch nests
/// further up instead of crossing the line, and the plan's §10.2
/// "alternating lanes" ordering (#1 up, #2 down, #3 two-up, #4 two-down)
/// falls out of it.
fn pick_lane(
    taken: &HashMap<i32, Vec<(u64, u64)>>,
    parent_lane: i32,
    from_x: u64,
    to_x: u64,
) -> i32 {
    let outward = |d: i32| -> (i32, i32) {
        let mag = parent_lane.abs() + d;
        if parent_lane > 0 {
            (mag, -mag)
        } else {
            (-mag, mag)
        }
    };
    let mut d = 1;
    loop {
        let (first, second) = outward(d);
        for cand in [first, second] {
            let free = taken
                .get(&cand)
                .map_or(true, |v| v.iter().all(|&(s, e)| to_x < s || e < from_x));
            if free {
                return cand;
            }
        }
        d += 1;
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

/// The tool ids that appear on **one side only, anywhere in the whole
/// log** — a call whose result has not been written yet (the agent is
/// inside a tool call as this projection runs), or a result whose call
/// is not in the log at all. Popping a rewind marker cannot repair
/// either: un-masking events can only bring back a counterpart that is
/// *in* the log. See [`strands_pair`].
fn unpaired_ids(events: &[Value]) -> HashSet<String> {
    let calls: HashSet<&str> = events
        .iter()
        .filter_map(|ev| ev.get("tool_calls").and_then(|c| c.as_array()))
        .flatten()
        .filter_map(|c| c.get("id").and_then(|i| i.as_str()))
        .collect();
    let results: HashSet<&str> = events
        .iter()
        .filter(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        .filter_map(|ev| ev.get("id").and_then(|i| i.as_str()))
        .collect();
    calls
        .symmetric_difference(&results)
        .map(|s| s.to_string())
        .collect()
}

/// The pair-stranding invariant (the kernel's
/// `context_strands_pairs`): the id of the first tool call whose
/// result is missing, or of the first result whose call is missing.
/// `None` when every pair is complete.
///
/// **v0.5.69 (round-3 defect 1)**: `pending` — the whole log's
/// [`unpaired_ids`] — names the ids that are unpaired *everywhere*,
/// and those are not strands at all while the session's loop is alive.
/// The kernel only ever runs this check from `bin/assemble`, which
/// assembles **between** turns: at that moment every call of the
/// previous turn has its result. A projection taken *while the agent
/// is working* would otherwise see the in-flight call as a strand,
/// pop every rewind marker, and draw the whole history linear — which
/// is exactly what the live sessions did. `None` restores the exact
/// kernel rule (a dead loop, or a settled log).
fn strands_pair(events: &[&Value], pending: Option<&HashSet<String>>) -> Option<String> {
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
    let is_pending = |id: &str| pending.is_some_and(|p| p.contains(id));
    for ev in events {
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("assistant_message") => {
                if let Some(calls) = ev.get("tool_calls").and_then(|c| c.as_array()) {
                    for c in calls {
                        if let Some(id) = c.get("id").and_then(|i| i.as_str()) {
                            if !result_ids.contains(id) && !is_pending(id) {
                                return Some(id.to_string());
                            }
                        }
                    }
                }
            }
            Some("tool_result") => {
                if let Some(id) = ev.get("id").and_then(|i| i.as_str()) {
                    if !call_ids.contains(id) && !is_pending(id) {
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
    rewind_verdict_live(events, target_seq, mode, false)
}

/// [`rewind_verdict`] for a live session: the same rule, but a tool
/// call whose result is not in the log yet is pending rather than
/// stranded (v0.5.69 — a user rewinding *while* the agent works must
/// not be refused for the agent's own in-flight call; see
/// [`strands_pair`]).
pub fn rewind_verdict_live(
    events: &[Value],
    target_seq: u64,
    mode: &str,
    live: bool,
) -> RewindVerdict {
    let ms = markers(events);
    let bounds = boundaries(events);
    let pending = pending_ids(events, live);
    verdict_with(events, target_seq, mode, &ms, &bounds, pending.as_ref())
}

/// The ids to treat as pending for this projection: the whole log's
/// unpaired ids, but only while `live`.
fn pending_ids(events: &[Value], live: bool) -> Option<HashSet<String>> {
    live.then(|| unpaired_ids(events))
}

/// The R3 annotation for one log line, over the whole log. The tree
/// calls [`restore_with`] instead (one parse for every node); this
/// entry point is the tested one.
#[allow(dead_code)]
pub fn restore_at(events: &[Value], node_seq: u64) -> Restore {
    let ms = markers(events);
    let bounds = boundaries(events);
    restore_with(events, node_seq, &ms, &bounds, None)
}

/// [`rewind_verdict`] with the parsed markers/boundaries supplied (the
/// tree computes both once for every node).
fn verdict_with(
    events: &[Value],
    target_seq: u64,
    mode: &str,
    ms: &[Marker],
    bounds: &[BoundaryRef],
    pending: Option<&HashSet<String>>,
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
    match strands_pair(&kept, pending) {
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
    pending: Option<&HashSet<String>>,
) -> Restore {
    if let RewindVerdict::StrandsPair { missing } =
        verdict_with(events, node_seq, "on", ms, bounds, pending)
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
    ignored_markers_with(events, None)
}

/// [`ignored_markers`] with the whole log's pending ids supplied
/// (v0.5.69): while the session's loop is alive, a call whose result
/// is nowhere in the log is pending, not a strand — see
/// [`strands_pair`].
pub fn ignored_markers_with(
    events: &[Value],
    pending: Option<&HashSet<String>>,
) -> Vec<IgnoredMarker> {
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
        let Some(missing) = strands_pair(&kept, pending) else {
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
            restore_with(&events, 6, &ms, &bounds, None),
            Restore::Framed { version: 1, from_seq: 4, to_seq: 6 }
        );
        assert_eq!(restore_with(&events, 1, &ms, &bounds, None), Restore::Raw);
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

    /// v0.5.69 (round-3 defect 1): while the session's loop is alive, a
    /// tool call whose result is **nowhere in the log** — the agent is
    /// inside that call as the projection runs — is pending, not
    /// stranded. Without the exemption the pop loop drops every rewind
    /// marker of the log and the tree goes linear, which is exactly what
    /// the live sessions did.
    #[test]
    fn an_in_flight_call_does_not_drop_markers_while_the_loop_is_live() {
        let events = vec![
            um("round one"),   // 1
            um("round two"),   // 2
            rw(1, "on"),       // 3 — back to round one
            um("round three"), // 4 — the branch that rewind made
            call("c2"),        // 5 — in flight: no result anywhere
        ];
        // The kernel's rule, blind to time (a dead loop): the open call
        // strands, the marker is dropped, the history is one line.
        let dead = build_live("s", &events, false);
        assert_eq!(
            dead.ignored,
            vec![IgnoredMarker {
                seq: 3,
                target_seq: 1,
                mode: "on".to_string(),
                missing: "c2".to_string(),
            }]
        );
        assert_eq!(dead.flow.orbit.fins, 0, "linear, as the kernel would project it");
        // The live rule: the call is pending, so the rewind stands and
        // round one keeps two children — the fork survives.
        let live = build_live("s", &events, true);
        assert!(live.ignored.is_empty(), "an in-flight call is not a strand");
        assert_eq!(live.tail_ignored, None);
        assert_eq!(live.flow.orbit.fins, 1);
        assert_eq!(live.flow.lanes, 1);
        assert_eq!(active_seqs(&live), vec![1, 4]);
    }

    /// The exemption is narrow: a pair the **mask** splits — the call is
    /// kept, its result sits in the log but outside the active ranges —
    /// is still a strand while the loop is live. That is the kernel's
    /// own rule and the plugin's parity promise.
    #[test]
    fn a_masked_but_logged_pair_still_strands_while_live() {
        let events = vec![
            um("round one"), // 1
            call("c1"),      // 2
            um("steer"),     // 3 — the pick that lands mid-step
            res("c1"),       // 4 — in the log, but masked by the rewind
            rw(2, "on"),     // 5
            um("round two"), // 6
            call("c2"),      // 7 — in flight
        ];
        assert_eq!(ignored_markers(&events)[0].missing, "c1");
        let live = build_live("s", &events, true);
        assert_eq!(live.ignored.len(), 1);
        assert_eq!(live.ignored[0].missing, "c1");
        assert_eq!(
            live.flow.orbit.fins, 0,
            "the pair the mask split still strands: only *unpaired* ids are pending"
        );
    }

    /// A pick is not refused because of the agent's own in-flight call
    /// (v0.5.69): the same pick that a dead loop would call
    /// `StrandsPair` is `Ok` while the loop is alive.
    #[test]
    fn a_pick_is_not_refused_for_an_in_flight_call_while_live() {
        let events = vec![
            um("round one"), // 1
            call("c2"),      // 2 — in flight
            um("round two"), // 3
        ];
        assert!(matches!(
            rewind_verdict(&events, 3, "on"),
            RewindVerdict::StrandsPair { .. }
        ));
        assert_eq!(rewind_verdict_live(&events, 3, "on", true), RewindVerdict::Ok);
        assert!(matches!(
            rewind_verdict_live(&events, 3, "on", false),
            RewindVerdict::StrandsPair { .. }
        ));
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

    // ── Style B: the flow projection (plan section 10) ───────────────

    /// No two segments may share a lane over the same column, and no two
    /// nodes may land on the same `(x, lane)`: the client multiplies both
    /// by pixels, so a collision is two dots drawn on top of each other.
    fn assert_flow_is_collision_free(tree: &RewindTree) {
        let bs = &tree.flow.branches;
        for (i, a) in bs.iter().enumerate() {
            for b in bs.iter().skip(i + 1) {
                if a.lane != b.lane {
                    continue;
                }
                assert!(
                    a.to_x < b.from_x || b.to_x < a.from_x,
                    "lane {} carries overlapping segments {}..{} and {}..{}",
                    a.lane,
                    a.from_x,
                    a.to_x,
                    b.from_x,
                    b.to_x
                );
            }
        }
        let mut seen = HashSet::new();
        for n in &tree.flow.nodes {
            assert!(
                seen.insert((n.x, n.lane)),
                "two nodes landed on ({}, {})",
                n.x,
                n.lane
            );
        }
    }

    /// `(seq, x, lane, main)` for every flow node, in round order.
    fn flow_rows(tree: &RewindTree) -> Vec<(u64, u64, i32, bool)> {
        tree.flow
            .nodes
            .iter()
            .map(|n| (n.seq, n.x, n.lane, n.main))
            .collect()
    }

    #[test]
    fn flow_of_a_straight_line_is_a_single_lane() {
        let tree = build("s", &[um("one"), um("two"), um("three")]);
        assert_eq!(tree.flow.cols, 3);
        assert_eq!(tree.flow.lanes, 0);
        assert_eq!(tree.flow.main_len, 3);
        assert_eq!(
            flow_rows(&tree),
            vec![(1, 0, 0, true), (2, 1, 0, true), (3, 2, 0, true)]
        );
        assert_eq!(tree.flow.edges.len(), 2);
        assert_eq!(tree.flow.branches.len(), 1);
        let b = &tree.flow.branches[0];
        assert_eq!((b.root, b.lane, b.from_x, b.to_x), (1, 0, 0, 2));
        assert_flow_is_collision_free(&tree);
    }

    #[test]
    fn flow_of_empty_and_single_round_logs() {
        let empty = build("s", &[]);
        assert!(empty.flow.nodes.is_empty());
        assert_eq!((empty.flow.cols, empty.flow.lanes, empty.flow.main_len), (0, 0, 0));

        let one = build("s", &[um("only")]);
        assert_eq!((one.flow.cols, one.flow.lanes, one.flow.main_len), (1, 0, 1));
        assert!(one.flow.nodes[0].main && one.flow.nodes[0].current);
        assert_flow_is_collision_free(&one);
    }

    /// A long straight log stays a straight log (the stress case: one
    /// column per round, no lanes).
    #[test]
    fn flow_of_a_long_straight_log() {
        let events: Vec<Value> = (0..200).map(|i| um(&format!("r{i}"))).collect();
        let tree = build("s", &events);
        assert_eq!(tree.flow.main_len, 200);
        assert_eq!(tree.flow.cols, 200);
        assert_eq!(tree.flow.lanes, 0);
        assert!(tree.flow.nodes.iter().all(|n| n.lane == 0 && n.main));
        assert_eq!(tree.flow.edges.len(), 199);
        assert_flow_is_collision_free(&tree);
    }

    /// The probe fixture's shape: rewind to round 2, then a new round
    /// becomes round 3's sibling. Both branches are one node long, so only
    /// the tie-break (the chain holding the current round) can choose the
    /// straight line.
    #[test]
    fn flow_forks_the_abandoned_branch_into_a_lane() {
        let events = vec![um("one"), um("two"), um("three"), rw(2, "on"), um("four")];
        let tree = build("s", &events);
        assert_eq!(tree.current_seq, Some(5));
        assert_eq!(tree.flow.main_len, 3);
        assert_eq!(
            flow_rows(&tree),
            vec![
                (1, 0, 0, true),   // round 1 — the line
                (2, 1, 0, true),   // round 2 — the fork
                (3, 2, -1, false), // round 3 — abandoned, one lane up
                (5, 2, 0, true),   // round 4 — the line continues (here)
            ]
        );
        assert_eq!((tree.flow.cols, tree.flow.lanes), (3, 1));
        // The fork is an elbow: the abandoned branch leaves the line.
        assert!(tree
            .flow
            .edges
            .iter()
            .any(|e| e.from == 2 && e.to == 3 && e.lane == -1 && !e.main));
        assert!(tree
            .flow
            .edges
            .iter()
            .any(|e| e.from == 2 && e.to == 5 && e.lane == 0 && e.main));
        let segs: Vec<(u64, i32, i32, u64, u64)> = tree
            .flow
            .branches
            .iter()
            .map(|b| (b.root, b.lane, b.parent_lane, b.from_x, b.to_x))
            .collect();
        assert_eq!(segs, vec![(1, 0, 0, 0, 2), (3, -1, 0, 2, 2)]);
        assert_flow_is_collision_free(&tree);
    }

    // ── v0.5.68: the orbital view's logical geometry ────────────────
    //
    // The ring's *angles* are the client's business (they depend on the
    // selection, which lives in the DOM), but the numbers a test can pin are
    // here: how many fins exist, in which order, where each hinges, and which
    // rounds belong to which fin.

    #[test]
    fn orbit_counts_the_fins_and_leaves_the_trunk_out_of_the_ring() {
        // a straight log has nothing to orbit
        let line = build("s", &[um("a"), um("b")]);
        assert_eq!(line.flow.orbit.fins, 0);
        assert_eq!((line.flow.orbit.step_deg, line.flow.orbit.arc_deg), (30, 60));
        assert!(line.flow.branches.iter().all(|b| b.fin.is_none()));

        // one fork → exactly one fin; the main line never takes a slot, and
        // the fin hinges on its *parent's* column (where it leaves the trunk)
        let tree = build("s", &[um("one"), um("two"), um("three"), rw(2, "on"), um("four")]);
        assert_eq!(tree.flow.orbit.fins, 1);
        assert!(tree
            .flow
            .branches
            .iter()
            .filter(|b| b.lane == 0)
            .all(|b| b.fin.is_none()));
        let fork = tree.flow.branches.iter().find(|b| b.lane != 0).unwrap();
        assert_eq!(fork.fin, Some(0));
        assert_eq!(fork.root, 3);
        assert_eq!(fork.hinge_x, 1); // round 2 sits at column 1 …
        assert_eq!(fork.from_x, 2); // … and its fork is the next column
        assert_eq!(fork.seqs, vec![3]);
        assert_eq!(tree.flow.orbit.fins, 1);
    }

    #[test]
    fn orbit_gives_every_fin_a_slot_in_branch_order() {
        let events = vec![
            um("a"),
            um("b"),
            um("c1"),
            rw(2, "on"),
            um("c2"),
            rw(2, "on"),
            um("c3"),
        ];
        let tree = build("s", &events);
        let slots: Vec<(u32, u64)> = tree
            .flow
            .branches
            .iter()
            .filter_map(|b| b.fin.map(|f| (f, b.root)))
            .collect();
        // two forks off round 2 → two fins, and the ring's order is the
        // server's branch order (so it is stable across reloads)
        assert_eq!(slots, vec![(0, 3), (1, 5)]);
        assert_eq!(tree.flow.orbit.fins, 2);
    }

    #[test]
    fn orbit_groups_every_round_into_exactly_one_branch() {
        // the nested case: the 2nd fin forks out of the 1st one, and both sit
        // in the same walk — the lane alone could not tell them apart, the
        // seqs can.
        let events = vec![
            um("r1"),
            um("r2"),
            um("r3"),
            rw(2, "on"),
            um("r4"),
            rw(3, "on"),
            um("r5"),
            rw(3, "on"),
            um("r6"),
            rw(5, "on"),
            um("r7"),
            um("r8"),
        ];
        let tree = build("s", &events);
        let mut seen: HashSet<u64> = HashSet::new();
        for b in &tree.flow.branches {
            for s in &b.seqs {
                assert!(seen.insert(*s), "seq {s} lands in two branches");
            }
        }
        let all: Vec<u64> = tree.flow.nodes.iter().map(|n| n.seq).collect();
        assert_eq!(seen.len(), all.len(), "a round belongs to no branch");
        assert!(all.iter().all(|s| seen.contains(s)));

        let trunk = tree.flow.branches.iter().find(|b| b.lane == 0).unwrap();
        assert_eq!(trunk.fin, None);
        assert_eq!(trunk.seqs, vec![1, 2, 5, 11, 12]);
        let outer = tree.flow.branches.iter().find(|b| b.root == 3).unwrap();
        assert_eq!((outer.fin, outer.hinge_x, &outer.seqs), (Some(0), 1, &vec![3, 7]));
        let nested = tree.flow.branches.iter().find(|b| b.root == 9).unwrap();
        assert_eq!((nested.fin, nested.hinge_x, &nested.seqs), (Some(1), 2, &vec![9]));
        assert_eq!(tree.flow.orbit.fins, 2);
    }

    /// The 1st branch off the line goes up, the 2nd down (§10.2) — they
    /// cover the same columns, so they cannot share a lane.
    #[test]
    fn flow_alternates_sibling_lanes_up_then_down() {
        let events = vec![
            um("a"),
            um("b"),
            um("c1"),
            rw(2, "on"),
            um("c2"),
            rw(2, "on"),
            um("c3"),
        ];
        let tree = build("s", &events);
        assert_eq!(
            flow_rows(&tree),
            vec![
                (1, 0, 0, true),
                (2, 1, 0, true),
                (3, 2, -1, false), // 1st fork off round 2 → up
                (5, 2, 1, false),  // 2nd fork → down
                (7, 2, 0, true),   // the current round continues the line
            ]
        );
        assert_eq!(tree.flow.lanes, 1);
        assert_flow_is_collision_free(&tree);
    }

    /// A fork *inside* a branch nests further out (`-1` → `-2`), i.e. away
    /// from the line, never across it.
    #[test]
    fn flow_nests_a_fork_inside_a_branch_further_out() {
        let events = vec![
            um("r1"),
            um("r2"),
            um("r3"),
            rw(2, "on"),  // round 3 leaves the active path …
            um("r4"),     // … and round 4 continues it
            rw(3, "on"),  // give round 3 two children
            um("r5"),
            rw(3, "on"),
            um("r6"),
            rw(5, "on"), // and make round 4's chain the tallest
            um("r7"),
            um("r8"),
        ];
        let tree = build("s", &events);
        assert_eq!(
            flow_rows(&tree),
            vec![
                (1, 0, 0, true),   // r1
                (2, 1, 0, true),   // r2 — the fork
                (3, 2, -1, false), // r3 — the abandoned branch, up one lane
                (5, 2, 0, true),   // r4 — the line
                (7, 3, -1, false), // r5 — keeps r3's lane …
                (9, 3, -2, false), // … r6 forks further out
                (11, 3, 0, true),  // r7
                (12, 4, 0, true),  // r8 — the current round
            ]
        );
        assert_eq!((tree.flow.cols, tree.flow.lanes, tree.flow.main_len), (5, 2, 5));
        let segs: Vec<(u64, i32, i32, u64, u64)> = tree
            .flow
            .branches
            .iter()
            .map(|b| (b.root, b.lane, b.parent_lane, b.from_x, b.to_x))
            .collect();
        assert_eq!(
            segs,
            vec![
                (1, 0, 0, 0, 4),    // the line
                (3, -1, 0, 2, 3),   // r3's branch
                (9, -2, -1, 3, 3), // r6 nested inside it
            ]
        );
        assert_flow_is_collision_free(&tree);
    }

    /// The D1 caveat, pinned: when an *abandoned* branch is longer than
    /// what followed the rewind, the longest chain — the straight line —
    /// is the abandoned one and the current round sits on a fork. This is
    /// what the user's "longest chain is the main line" means.
    #[test]
    fn flow_lets_an_abandoned_branch_be_the_longest_line() {
        let events = vec![
            um("a"),
            um("b"),
            um("c"),
            um("d"),
            rw(2, "on"),
            um("back"),
        ];
        let tree = build("s", &events);
        assert_eq!(tree.current_seq, Some(6));
        assert_eq!(tree.flow.main_len, 4); // a,b,c,d — not the live tail
        assert_eq!(
            flow_rows(&tree),
            vec![
                (1, 0, 0, true),
                (2, 1, 0, true),
                (3, 2, 0, true),
                (4, 3, 0, true),  // the abandoned tail is the straight line
                (6, 2, -1, false), // the current round hangs one lane up
            ]
        );
        let cur = tree.flow.nodes.iter().find(|n| n.current).unwrap();
        assert_eq!((cur.lane, cur.main), (-1, false));
        assert_eq!(cur.restore, tree.flow.nodes[4].restore.clone());
        assert_flow_is_collision_free(&tree);
    }

    // ── B2: the per-round detail ─────────────────────────────────────

    #[test]
    fn node_detail_returns_the_full_message() {
        let long = "first line\nsecond line \u{2014} with \"quotes\" and,; punctuation".to_string();
        let events = vec![um("short one"), um(&long), am(), ext()];
        let d = node_detail(&events, 2).expect("round 2 is a user message");
        assert_eq!(d.seq, 2);
        assert_eq!(d.round, 2);
        assert_eq!(d.text, long); // verbatim, newlines and all — never summarized
        assert_ne!(d.text, summarize(&long));
        // The event count is the round's span minus `ext_status`, and the
        // round's own `user_message` is part of it (see
        // `round_metadata_and_event_folding`): um + am here, the ext not.
        assert_eq!(d.events, 2);
        assert_eq!(d.state, "active");
        assert!(d.current);
        assert!(!d.retracted);
        // round 1 carries no folded events, and the summary differs
        let d1 = node_detail(&events, 1).unwrap();
        assert_eq!(d1.text, "short one");
        assert_eq!(d1.events, 1); // its own message, nothing folded in
        assert!(!d1.current);
    }

    #[test]
    fn node_detail_rejects_lines_that_are_not_a_round() {
        let events = vec![um("one"), am(), um("two")];
        assert!(node_detail(&events, 2).is_none(), "an assistant message");
        assert!(node_detail(&events, 0).is_none(), "line 0");
        assert!(node_detail(&events, 4).is_none(), "past the end");
        assert!(node_detail(&[], 1).is_none(), "an empty log");
    }

    #[test]
    fn node_detail_reports_an_abandoned_or_retracted_round() {
        let events = vec![
            um("one"),
            um("two"),
            um("three"),
            rw(2, "on"),
            um("four"),
            um_id("five", "id5"),
            json!({ "v": 1, "type": "user_message_retract", "ts": "t", "target": "id5" }),
        ];
        // round 3 was masked by the rewind to round 2
        let three = node_detail(&events, 3).unwrap();
        assert_eq!(three.text, "three");
        assert_eq!(three.state, "abandoned");
        assert!(!three.current);
        // round 6 is the current one, and it was retracted
        let six = node_detail(&events, 6).unwrap();
        assert_eq!(six.state, "active");
        assert!(six.current);
        assert!(six.retracted);
    }

    /// The flow's per-node text is the same one style A shows, so the
    /// detail panel and the list cannot disagree.
    #[test]
    fn flow_nodes_carry_the_detail_the_panel_shows() {
        let events = vec![um("hello world"), am(), ext()];
        let tree = build("s", &events);
        let n = &tree.flow.nodes[0];
        assert_eq!(n.summary, "hello world");
        assert_eq!(n.events, 2); // the assistant message + the ext_status
        assert_eq!(n.round, 1);
        assert!(n.current && n.main);
        assert_eq!(n.restore, Restore::Raw);
    }
}
