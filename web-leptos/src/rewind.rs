//! Rewind plugin (client): the conversation history tree.
//!
//! One data source (`GET /api/sessions/{id}/rewind`) feeds two surfaces:
//!   * the **full-window History view** — the rebuilt `layout-full` layout:
//!     a session rail on the left (the dispatch-view session cards, grouped
//!     by working path, with their loop start/stop toggle), the recursive
//!     round/branch tree on the right, one node per user message (one loop
//!     round);
//!   * the **sidebar plugin panel** (`#plugin-area`, registered in
//!     `plugins.rs`) — a compact summary + the active path.
//!
//! A node click opens [`RewindConfirmDialog`] ("Rewind to this point?") and,
//! on confirm, writes `rewind { target_seq, mode:"on" }`: the target user
//! message stays the active tail, everything after it moves to an abandoned
//! branch that remains in the append-only log — and in the tree — and can be
//! re-entered by another rewind.
//!
//! Rewind is **forbidden while the session's loop is running**: the card
//! quick button is disabled and the tree stays viewable but not clickable
//! ([`rewind_allowed`]).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, MouseEvent, PointerEvent, ResizeObserver};

use crate::api;
use std::collections::HashMap;

use crate::model::{
    AppState, FlowBranch, FlowNode, Restore, RewindNode, RewindTarget, SessionInfo,
};
use crate::timeutil;

/// Whether a rewind targeting `session` is allowed right now. Never while
/// that session's loop runs: an in-flight turn would land inside the fresh
/// branch, so the action is refused until the loop settles.
pub fn rewind_allowed(state: AppState, session: &str) -> bool {
    !(state.loop_running.get() || state.looping_sessions.get().contains(session))
}

/// Open the rewind confirm dialog for one target.
pub fn request_rewind(state: AppState, seq: u64, label: String) {
    state.rewind_pending.set(Some(RewindTarget { seq, label }));
}

/// One tree fetch for the whole app, keyed on the active session and the
/// structure-generation counter (`ws.rs` bumps it on `user_message` /
/// `rewind` frames only). Both surfaces read `state.rewind_tree`.
pub fn register_tree_effect(state: AppState) {
    let sess = state.active_session;
    let gen = state.rewind_gen;
    let tree = state.rewind_tree;
    Effect::new(move || {
        let Some(s) = sess.get() else {
            tree.set(None);
            return;
        };
        let _ = gen.get();
        spawn_local(async move {
            match api::load_rewind_tree(&s).await {
                Ok(t) => tree.set(Some(t)),
                // A failed fetch/parse used to vanish into the "loading…"
                // placeholder; keep the reason reachable (it is how the
                // v0.5.65 `missing`-less payload was found).
                Err(e) => {
                    web_sys::console::warn_1(&format!("rewind tree ({s}): {e}").into());
                    tree.set(None);
                }
            }
        });
    });
}

// ── Style B (the "flow" view): which style, and what is selected ───
//
// v0.5.67: the flow style is the default; the list is opt-in from the
// History top bar and the choice survives a reload. D5: opening it selects
// the round the session is at, so the detail panel is never empty. The
// selection is client-only and lives with the rest of the plugin state
// (`AppState::rw_selected`); clicking a node in the scene only sets it —
// the Rewind button in the detail panel is the only trigger there.

/// The localStorage key holding the History style ("tree" | "flow").
const VIEW_KEY: &str = "rushi-rw-view";

/// The persisted style: `"flow"` or `"tree"`. **`flow` is the default**
/// (user decision 2026-10-03, superseding D2's "list is the default"): the
/// scene draws every session measured, while the recursive list cannot
/// render the long ones at all (see `nodes_view`). Anything unknown falls
/// back to the flow style.
pub fn read_view_mode() -> String {
    let stored = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item(VIEW_KEY).ok())
        .flatten();
    match stored.as_deref() {
        Some("tree") => "tree".to_string(),
        _ => "flow".to_string(),
    }
}

/// Switch style and remember it (a display-only preference: no server call).
pub fn set_view_mode(state: AppState, mode: &str) {
    state.rw_view.set(mode.to_string());
    if let Some(s) = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
    {
        let _ = s.set_item(VIEW_KEY, mode);
    }
}

/// Select a round in the flow view: the detail panel follows, and **no
/// rewind happens** (the user's rule for the new style).
pub fn select_node(state: AppState, seq: u64) {
    state.rw_selected.set(Some(seq));
}

thread_local! {
    /// The one `resize` listener Style B installs. A resize is a layout
    /// point (not a scroll frame), so the scene re-measures itself here.
    static SCENE_RESIZE: RefCell<Option<Closure<dyn Fn()>>> = const { RefCell::new(None) };
    /// **bug 3 (D-cone-12, user decision, v0.5.71)**: the scene's own layout
    /// probe — one `ResizeObserver` on the element the scene is mounted at.
    ///
    /// Mounting is *not* a reactive event: when the History opens, none of the
    /// flow effect's dependencies (`view`/`tree`/`selected`) changes, and its
    /// `Timeout(0)` has already fired while the scene was still absent (the
    /// split plugin body fetches the tree, so `sync_scene_metrics` returned
    /// early and never measured). The scene therefore used to paint with **no
    /// geometry at all** — `--q`/`--cellpx`/`--axis` empty — which fell back to
    /// a plain horizontal line, i.e. "the previous version's look", until some
    /// later event (a pointer press, a tree refetch) measured it.
    ///
    /// A `ResizeObserver` fires once when it is observed and on every box
    /// change after that, which is exactly the set of layout points the
    /// geometry needs: the mount, the split↔full switch, the sidebar, the
    /// panel. One slot: the probe is re-armed (and the old one disconnected)
    /// whenever the measured element changes.
    static SCENE_OB: RefCell<Option<(ResizeObserver, Closure<dyn FnMut(js_sys::Array)>)>> =
        const { RefCell::new(None) };
    /// The element `SCENE_OB` currently watches, for the identity compare.
    static SCENE_OB_EL: RefCell<Option<HtmlElement>> = const { RefCell::new(None) };
    /// **v0.5.69 (round-3 defect 2)**: the turn a gesture could not spend
    /// on panning. `--rw-scroll` is `scrollLeft + this`, so a scene narrower
    /// than its panel (which is what a track of 17..45 columns is,
    /// `#rw-flow-track`'s `--cell` makes it fit exactly) still turns the cone
    /// even though it has no scroll range at all. Since v0.5.73 the wheel does
    /// not pan at all — it *walks the fan* ([`snap_step`]) — and this is the
    /// accumulator the snap flights and the drag write through. Reset at every
    /// layout, like `--rw0`.
    static RW_TURN: Cell<f64> = const { Cell::new(0.0) };
    /// **v0.5.73 (plan §15)**: the running snap. One at a time: a new gesture
    /// cancels the old one and retargets, so a fast wheel cannot queue a
    /// backlog of flights.
    static RW_SNAP: RefCell<Option<gloo_timers::callback::Interval>> =
        const { RefCell::new(None) };
}

/// The scene's turn accumulator (px, the same unit as `scrollLeft`).
fn turn() -> f64 {
    RW_TURN.with(|c| c.get())
}
fn set_turn(v: f64) {
    RW_TURN.with(|c| c.set(v));
}

/// A computed custom property as a number (the layout's own numbers are all
/// written without units except the lengths, which we never read this way).
fn css_num(el: &HtmlElement, name: &str) -> Option<f64> {
    let w = web_sys::window()?;
    let cs = w.get_computed_style(el).ok().flatten()?;
    cs.get_property_value(name)
        .ok()?
        .trim()
        .trim_end_matches("px")
        .parse::<f64>()
        .ok()
}

/// Wrap into (-180, 180]: the cone's angles are all read that way, so a fan's
/// angle does not depend on how many turns the phase has behind it.
fn wrap180(deg: f64) -> f64 {
    (deg + 180.0).rem_euclid(360.0) - 180.0
}

/// `ease-in-out`, so the snap leaves and arrives softly.
fn ease01(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Can we animate at all? `prefers-reduced-motion` jumps straight to the
/// detent (D-snap-4) — and it is also the flat projection's switch.
fn reduced_motion() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten())
        .map(|m| m.matches())
        .unwrap_or(false)
}

/// Abandon a flight in progress (a new gesture retargets instead of queuing).
fn cancel_snap() {
    RW_SNAP.with(|s| {
        if let Some(iv) = s.borrow_mut().take() {
            iv.cancel();
        }
    });
}

/// **The focus carousel's snap (plan §15.4, D-snap-3/4)**: fly the cone to the
/// **next detent in the direction of travel** and, on arrival, commit that
/// branch's rank as the focus — so the new law re-hangs the whole fan with the
/// new branch on top, and the phase returns to zero.
///
/// The rule is "strictly ahead", never "the nearest": a gesture that lands
/// exactly on a detent still advances one branch, which is the v0.5.69 rule
/// ("a gesture must never be a no-op") and the user's own words — *"每一次滚轮
/// 滚动，都会让上方的分支回到下方并让一个新的分支转动到上方并吸附"*.
///
/// The flight moves `--rw-scroll` only (the single value a frame writes, B7),
/// through the turn accumulator, so nothing reads layout per frame. `dir` is
/// ±1. A no-op is impossible: with `dir > 0` the target phase is the smallest
/// detent strictly above the current one, and the detents wrap, so there is
/// always one.
fn snap_step(state: AppState, el: &HtmlElement, dir: i32, k: usize, focus: i32) {
    if k == 0 {
        return;
    }
    // Stop the flight in the air (a fast wheel retargets instead of queuing).
    cancel_snap();
    let detent = |rank: i32| wrap180(fan_root_theta(rank, focus, k));
    let dpp = css_num(el, "--dpp").unwrap_or(0.3).max(1e-6);
    // where the cone is, in **degrees** (`--rw-scroll − --rw0` is px; `dpp` is
    // degrees per px). It is deliberately not wrapped: the detents are compared
    // modulo 360 below.
    let phase_now = (css_num(el, "--rw-scroll").unwrap_or(0.0)
        - css_num(el, "--rw0").unwrap_or(0.0))
        * dpp;
    // The detents in *phase* degrees: a branch is on top when its angle plus
    // the phase is zero.
    let mut target: Option<(f64, i32)> = None;
    for j in 0..k as i32 {
        let want = -detent(j);
        // how far ahead of where we are, in the direction of travel
        let mut delta = (want - phase_now).rem_euclid(360.0);
        if dir < 0 {
            delta -= 360.0;
        }
        if dir > 0 && delta <= 1e-9 {
            delta += 360.0; // *strictly* ahead: a landed gesture still moves
        }
        if dir < 0 && delta >= -1e-9 {
            delta -= 360.0;
        }
        if target.is_none() || (dir > 0 && delta < target.unwrap().0)
            || (dir < 0 && delta > target.unwrap().0)
        {
            target = Some((delta, j));
        }
    }
    let Some((delta, rank)) = target else { return };
    let rw0 = css_num(el, "--rw0").unwrap_or(0.0);
    let from = el.scroll_left() as f64 + turn();
    let to = rw0 + (phase_now + delta) / dpp;
    if reduced_motion() {
        set_turn(to - el.scroll_left() as f64);
        mark_scroll(el);
        finish_snap(state, el, rank, rw0);
        return;
    }
    let t0 = web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0);
    let el2 = el.clone();
    let iv = gloo_timers::callback::Interval::new(16, move || {
        let now = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(t0);
        let t = ((now - t0) / SNAP_MS).clamp(0.0, 1.0);
        let v = from + (to - from) * ease01(t);
        set_turn(v - el2.scroll_left() as f64);
        mark_scroll(&el2);
        if t >= 1.0 {
            cancel_snap();
            finish_snap(state, &el2, rank, rw0);
        }
    });
    RW_SNAP.with(|s| *s.borrow_mut() = Some(iv));
}

/// Arrive: the new branch is the focus (so the render re-hangs the fan with it
/// on top) and the phase goes back to rest. For a fan of up to three branches
/// the new law is the old one rotated by exactly the angle we just flew, so the
/// picture does not move a pixel here; the even fan (4+) is rigid by
/// construction and is smooth too.
fn finish_snap(state: AppState, el: &HtmlElement, rank: i32, rw0: f64) {
    state.rw_focus.set(rank);
    set_turn(rw0 - el.scroll_left() as f64);
    mark_scroll(el);
}

/// B7's numbers, written once per **layout** so that a scroll frame writes
/// exactly one value.
///
/// Each ribbon's `--turn` is a pure CSS `calc()` over three plane numbers —
/// its own centre (`--bx`), the offset (`--rw-scroll`, the only thing a
/// scroll frame touches) and half the viewport plus the falloff width
/// (`--halfpw`, `--denom`). Numbers, never lengths: CSS cannot divide a
/// length by a length, and this way the proxy costs one `setProperty` per
/// scroll event and never reads layout while scrolling.
/// **D-snap-5 (user decision, 2026-10-05, plan §15)**: where the trunk sits in
/// the scene, as a fraction of its height. The focus carousel puts the focused
/// branch **on top** and every other branch in the lower half, so the room
/// above the trunk is what the focused branch needs and the room below is only
/// the (much smaller) unfocused arcs. Moving the axis down is what buys the
/// focused branch its length: on the live 418px panel the fit goes from
/// `q 25.7px` to `q 36.5px` (+42%, measured), and the unfocused branches are
/// drawn at `--kof` (0.82 there) so they still fit below.
///
/// Measured, and why not lower: at 0.72 the lower room is so short that `--kof`
/// drops to 0.65, and then the *first* bead of a branch at the arc's end sits
/// 12.7px from the trunk's own bead in the same column — a hair inside one dot
/// (measured 7.7/9.2px apart). At 0.68 `--kof` is 0.82 and that distance is
/// 15px, clear, for 8px less reach on the focused branch.
const SCENE_AXIS_FRAC: f64 = 0.68;
/// Half the bead band in px (the CSS gives a `.rw3-dot` a 13px dot).
const FIN_HALF_PX: f64 = 13.0;
/// Air between the farthest bead's end and the panel's edge.
const FIN_MARGIN_PX: f64 = 16.0;
/// A cone smaller than this reads as a squashed line; below it the fit gives
/// way rather than the geometry.
const FIN_MIN_RADIUS_PX: f64 = 34.0;
/// **D-orb-8**: one full turn per 1.5 scene widths of scrolling — a fixed
/// angular rate, so the gesture feels the same in every session.
const ORBIT_DEG_PER_WIDTH: f64 = 360.0 / 1.5;
/// **D-snap-1 (user decision, 2026-10-05)**: how far the *lower* arc stays off
/// the horizontal. A branch at ±90° is **edge-on** (its screen offset is
/// `-q·cos a`: nothing of the ray is left on screen and its beads land on the
/// trunk row — the §14.1 K2 class, measured 1.1-3.6px from a trunk bead), so
/// the non-focused branches start `FAN_ARC_INSET` degrees below the horizontal
/// and end as far above it on the other side. At `k = 3` this is exactly
/// today's 120°/240°; it only bites for larger fans.
const FAN_ARC_INSET: f64 = 30.0;
/// **D-snap-5**: an unfocused branch may not shrink below this or it stops
/// reading as a branch; the fit gives way instead.
const FAN_MIN_SCALE: f64 = 0.45;
/// **D-snap-9 / §14.4 B (user decision)**: the cone's opening should lean
/// **up-right** instead of straight right, so the focused branch's far end runs
/// up and to the right.
///
/// **Measured (v0.5.73): the obvious mechanism is wrong.** A `rotateZ` on the
/// branch's own plane does lean the ray, but it rolls the branch's **column
/// axis** with it: the beads of a 7-column branch then span `±7·cell·sin ψ`
/// (41px at 10°) of height, which tilted the branch's bead line and lifted its
/// far beads *above* the trunk row — the probe caught beads on the wrong side
/// of the axis. The tilt has to shear the *radial* direction only (each step
/// leans, the columns stay horizontal), which is the plan's D-slant question;
/// until that lands this is 0 and the CSS machinery
/// (`--roll`, the bead's `rotateZ(-ψ)`) is wired and inert.
const CONE_ROLL_DEG: f64 = 0.0;
/// **D-snap-4**: the snap's flight time. A gesture must never be a no-op
/// (v0.5.69), so every settle rotates at least one branch up.
const SNAP_MS: f64 = 300.0;

/// Keep the scene's layout probe on `scroll` (bug 3 / D-cone-12). Cheap: the
/// element is compared by identity, and an already-armed scene does nothing.
/// A re-render that replaces the node re-arms the observer on the new one.
fn arm_scene_probe(scroll: &HtmlElement) {
    SCENE_OB_EL.with(|slot| {
        if slot.borrow().as_ref() == Some(scroll) {
            return;
        }
        SCENE_OB.with(|obs| {
            let mut obs = obs.borrow_mut();
            if let Some((old, _)) = obs.take() {
                old.disconnect();
            }
            let cb = Closure::<dyn FnMut(js_sys::Array)>::new(|_: js_sys::Array| {
                relayout_scene()
            });
            if let Ok(ro) =
                ResizeObserver::new(cb.as_ref().unchecked_ref::<js_sys::Function>())
            {
                ro.observe(scroll);
                *obs = Some((ro, cb));
            }
        });
        slot.borrow_mut().replace(scroll.clone());
    });
}

/// The scene's **mount** layout point: arm the probe on the element that is
/// there right now and measure once, so the first paint is the cone rather
/// than the horizontal fallback. Returns false when the scene is not in the
/// DOM yet (the tree is still loading), where the tree effect's own
/// `relayout_scene` will arm it later.
fn arm_scene_probe_now() -> bool {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    let Some(scroll) = doc
        .query_selector("#rw-flow-scroll")
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
    else {
        return false;
    };
    arm_scene_probe(&scroll);
    relayout_scene();
    true
}

fn sync_scene_metrics() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(scroll) = doc
        .query_selector("#rw-flow-scroll")
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
    else {
        return;
    };
    arm_scene_probe(&scroll);
    let w = scroll.client_width().max(1) as f64;
    let h = scroll.client_height().max(1) as f64;
    let half = w / 2.0;
    let st = scroll.style();
    let _ = st.set_property("--halfpw", &format!("{half}"));
    let _ = st.set_property("--denom", &format!("{}", half * 1.1));
    // the same value `mark_scroll` writes, turn included: a re-measure must
    // never move the ring
    mark_scroll(&scroll);

    // ── the orbital scene's pixels ─────────────────────────────────
    // **D-snap-5 (plan §15)**: with the focused branch on top and every other
    // branch in the lower half, the two rooms are no longer equal — the focused
    // branch spends `axis` (SCENE_AXIS_FRAC of the panel) and the unfocused
    // ones the rest, drawn at `--kof`. On the live 418px panel that is
    // `q 25.7px → 38.9px` for the focused branch (+51%) and `--kof 0.65` for
    // the others. The roll (D-snap-9) leans each ray, so its vertical reach is
    // its length times `cos ψ`.
    let axis = (h * SCENE_AXIS_FRAC).round();
    let up_room = (axis - FIN_MARGIN_PX - FIN_HALF_PX).max(FIN_MIN_RADIUS_PX);
    let down_room = ((h - axis) - FIN_MARGIN_PX - FIN_HALF_PX).max(0.0);
    let roll = CONE_ROLL_DEG.to_radians().cos();
    let _ = st.set_property("--axis", &format!("{axis}px"));
    let _ = st.set_property("--roll", &format!("{CONE_ROLL_DEG}"));
    // Every branch shares one slope, and the *longest* branch sets it: with the
    // axis low, the focused branch's farthest bead spends the whole upper room
    // (D-cone-8/9's auto-fit, now asymmetric). The length is read back off the
    // DOM (each container carries `data-n`), so the slope stays a layout-time
    // pixel value and the CSS keeps owning the cell.
    let longest = doc
        .query_selector_all(".rw-branch")
        .ok()
        .map(|list| {
            (0..list.length())
                .filter_map(|i| list.item(i))
                .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
                .filter_map(|el| el.get_attribute("data-n"))
                .filter_map(|s| s.parse::<f64>().ok())
                .fold(0.0_f64, f64::max)
        })
        .unwrap_or(0.0);
    let q = if longest > 0.0 {
        up_room / (longest * roll)
    } else {
        0.0
    };
    let _ = st.set_property("--q", &format!("{q:.3}px"));
    let _ = st.set_property("--r", &format!("{:.3}px", q * longest));
    // **D-snap-5**: how much an *unfocused* branch shrinks so the lower half
    // holds them all. The worst branch is the one whose plane leans most into
    // the screen (`max |cos θ|`), and that is what the render wrote on each
    // root container — read it back like `longest`, so the number always
    // matches the painted angles.
    let cos_worst = doc
        .query_selector_all(".rw-orbit > .rw-branch")
        .ok()
        .map(|list| {
            (0..list.length())
                .filter_map(|i| list.item(i))
                .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
                .filter(|el| el.get_attribute("data-focus").as_deref() != Some("1"))
                .filter_map(|el| el.get_attribute("data-th"))
                .filter_map(|s| s.parse::<f64>().ok())
                .map(|d| d.to_radians().cos().abs())
                .fold(0.0_f64, f64::max)
        })
        .unwrap_or(0.0);
    let kof = if q > 0.0 && cos_worst > 0.0 {
        (down_room / (q * longest * roll * cos_worst)).clamp(FAN_MIN_SCALE, 1.0)
    } else {
        1.0
    };
    let _ = st.set_property("--kof", &format!("{kof:.4}"));
    // ── v0.5.70: the cell, in plain px, for the spine ───────────────
    // The spine's angle is `atan2(steps·q, columns·cell)` — and Chromium
    // refuses a *container unit* inside `atan2()` (measured: `--cell` is
    // `clamp(26px, calc(100cqw / N - 4px), 64px)`, which `hypot()` accepts
    // but `atan2()` treats as invalid, leaving the bar unrotated). So the
    // layout hands the CSS a plain length, *measured* off a segment that
    // already carries the real cell — never recomputed from a formula the
    // stylesheet would have to agree with.
    let cell_px = doc
        .query_selector(".rw3-seg")
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::Element>().ok())
        .and_then(|e| {
            web_sys::window()
                .and_then(|w| w.get_computed_style(&e).ok())
                .flatten()
        })
        .and_then(|cs| cs.get_property_value("width").ok())
        .and_then(|w| w.trim().trim_end_matches("px").parse::<f64>().ok());
    match cell_px {
        Some(c) => {
            let _ = st.set_property("--cellpx", &format!("{c:.3}px"));
        }
        None => {
            let _ = st.remove_property("--cellpx");
        }
    }
    // D-orb-8: a fixed angular rate, and `--rw0` = the offset the scene is at
    // when the layout runs, so entering the scene (or picking a round, or a
    // new round arriving) puts the ring back at rest with the aligned fin in
    // front — "the alignment rule always wins at rest".
    let _ = st.set_property("--dpp", &format!("{}", ORBIT_DEG_PER_WIDTH / w));
    let _ = st.set_property("--rw0", &format!("{}", scroll.scroll_left()));
}

/// A **layout point** (v0.5.69): re-measure the scene and put the ring back
/// at rest. The turn resets with `--rw0`, so what the user sees on entering
/// the view, on picking a round, or on a resize is the aligned fin at the
/// front (the D-orb-7 rule). A bare [`sync_scene_metrics`] is only a
/// re-measure (`on_down`) and must leave the ring exactly where it is.
fn relayout_scene() {
    set_turn(0.0);
    sync_scene_metrics();
}

/// Style B's two effects, registered once from `lib.rs` next to
/// [`register_tree_effect`]:
///
/// 1. keep a valid selection — reset on a session switch, then default to
///    the current round (D5); a round the user picked stays picked across
///    tree reloads (a new round arriving must not steal the panel);
/// 2. fetch the selected round in full (B2) whenever the selection changes
///    — and only while the flow view is showing, so style A makes no
///    useless request.
pub fn register_flow_effects(state: AppState) {
    let sess = state.active_session;
    let tree = state.rewind_tree;
    let view = state.rw_view;
    let selected = state.rw_selected;
    let detail = state.rw_detail;
    let layout = state.layout_mode;

    // B7: a new scene (or a resized panel) re-measures the ribbons. The
    // first pass runs after the render that produced them.
    // **bug 3**: `layout` is in the dependencies because *opening the History*
    // is a layout change and nothing else — without it this effect never sees
    // the scene appear (the scene arms its own probe too; see
    // [`arm_scene_probe`], this is the second line of defence).
    Effect::new(move || {
        let _ = (view.get(), tree.get(), selected.get(), layout.get());
        // **v0.5.73 (plan §15)**: a layout point also puts the *walk* back to
        // the start — the aligned branch is the one on top at rest (the
        // D-orb-7 rule, now read as "the focus is the selection's branch"), so
        // entering the view, picking a round or a tree reload always opens on a
        // clean, snapped scene.
        state.rw_focus.set(0);
        relayout_scene();
        gloo_timers::callback::Timeout::new(0, relayout_scene).forget();
    });
    SCENE_RESIZE.with(|slot| {
        if slot.borrow().is_some() {
            return;
        }
        let cb = Closure::<dyn Fn()>::new(relayout_scene);
        if let Some(w) = web_sys::window() {
            let _ = w.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        }
        *slot.borrow_mut() = Some(cb);
    });

    let last_sess: StoredValue<Option<String>> = StoredValue::new(None);
    Effect::new(move || {
        let s = sess.get();
        if last_sess.get_value() != s {
            last_sess.set_value(s);
            selected.set(None);
            detail.set(None);
        }
        if view.get() != "flow" {
            return;
        }
        let Some(t) = tree.get() else { return };
        let known = selected
            .get()
            .is_some_and(|seq| t.flow.nodes.iter().any(|n| n.seq == seq));
        if !known {
            selected.set(t.current_seq);
        }
    });

    Effect::new(move || {
        if view.get() != "flow" {
            return;
        }
        let (Some(seq), Some(s)) = (selected.get(), sess.get()) else {
            detail.set(None);
            return;
        };
        spawn_local(async move {
            match api::load_rewind_detail(&s, seq).await {
                Ok(d) => detail.set(Some(d)),
                Err(e) => {
                    web_sys::console::warn_1(&format!("rewind round {seq} ({s}): {e}").into());
                    detail.set(None);
                }
            }
        });
    });
}

/// Display label of a node ("round 3 · first chars…").
pub fn node_label(n: &RewindNode) -> String {
    format!("round {} \u{00b7} {}", n.round, n.summary)
}

// ── the full-window History view (the rebuilt layout-full) ─────────

/// The new full-window interface: top bar, session rail, tree, footer hint.
pub fn history_view(state: AppState) -> AnyView {
    let active = state.active_session;
    let layout = state.layout_mode;
    let theme_mode = state.theme_mode;

    view! {
        <div id="history-view">
            <div id="hist-top">
                <button
                    id="hist-back"
                    title="back to the chat"
                    on:click=move |_| {
                        crate::ui::set_layout_mode("split");
                        layout.set("split".to_string());
                    }
                >
                    { "\u{2039} back to chat" }
                </button>
                <div id="hist-title">
                    <span class="hist-head">{ "history" }</span>
                    <span class="hist-sess">
                        { move || active.get().unwrap_or_else(|| "no session".to_string()) }
                    </span>
                </div>
                <span
                    class=move || {
                        let running = active
                            .get()
                            .map(|s| {
                                state.loop_running.get()
                                    || state.looping_sessions.get().contains(s.as_str())
                            })
                            .unwrap_or(false);
                        if running { "rw-lamp running" } else { "rw-lamp" }
                    }
                    title=move || {
                        let running = active
                            .get()
                            .map(|s| {
                                state.loop_running.get()
                                    || state.looping_sessions.get().contains(s.as_str())
                            })
                            .unwrap_or(false);
                        if running {
                            "the loop is running \u{2014} rewind is disabled".to_string()
                        } else {
                            "the loop is idle".to_string()
                        }
                    }
                ></span>
                { hist_style_switch(state) }
                <button
                    id="hist-theme"
                    title=move || match theme_mode.get().as_str() {
                        "light" => "theme: light \u{2014} click for dark".to_string(),
                        "dark" => "theme: dark \u{2014} click for auto".to_string(),
                        _ => "theme: auto (follows system) \u{2014} click for light".to_string(),
                    }
                    on:click=move |_| {
                        let next = match theme_mode.get().as_str() {
                            "auto" => "light",
                            "light" => "dark",
                            _ => "auto",
                        };
                        theme_mode.set(next.to_string());
                        crate::ui::set_theme_mode_stored(next);
                        crate::ui::theme_apply(state);
                    }
                >
                    { move || crate::ui::theme_icon(theme_mode.get().as_str()) }
                </button>
            </div>
            <div id="hist-body">
                <div id="hist-rail">{ hist_rail(state) }</div>
                <Show
                    when=move || state.rw_view.get() == "flow"
                    fallback=move || view! { <div id="hist-tree">{ hist_tree(state) }</div> }
                >
                    { flow_view(state) }
                </Show>
            </div>
            <div id="hist-foot">{ move || hist_hint(state) }</div>
        </div>
    }
    .into_any()
}

/// Footer hint: what a click does, or why it is locked.
fn hist_hint(state: AppState) -> String {
    let Some(sess) = state.active_session.get() else {
        return "select a session".to_string();
    };
    if !rewind_allowed(state, &sess) {
        "rewind is disabled while the loop is running".to_string()
    } else {
        "click a node to rewind \u{00b7} disabled while the loop runs".to_string()
    }
}

/// The session rail: the dispatch-view **session cards, grouped by working
/// path** (v0.5.56, the C2 decision keeps the rail inside the full-window
/// view). It reuses the sidebar's card wholesale — `ui::session_card` (name,
/// last-output time, the per-session `\u{25B6} start` / `\u{25A0} stop` loop
/// toggle, the `\u{2026}` rename/delete menu) and `ui::session_group_head`
/// (the project directory + count) — with `stay = true`, so clicking a card
/// switches the tree's session in place instead of dropping out of the
/// History view.
///
/// The grouping key is the session's `cwd` marker (its working directory),
/// i.e. one group per project; sessions without a marker land in
/// "(no project)".
fn hist_rail(state: AppState) -> AnyView {
    let sessions = state.sessions;
    let sort_mode = state.sort_mode;
    let custom_order = state.custom_order;
    let output_rank = state.output_rank;

    view! {
        <For
            each=move || {
                crate::model::dispatch_groups(
                    &sessions.get(),
                    &sort_mode.get(),
                    &custom_order.get(),
                    &output_rank.get(),
                )
            }
            key=|g: &(String, Vec<SessionInfo>)| {
                let mut k = g.0.clone();
                k.push_str("::");
                for s in &g.1 {
                    k.push_str(&s.name);
                    k.push(',');
                }
                k
            }
            children=move |g: (String, Vec<SessionInfo>)| {
                let key = g.0.clone();
                let items = g.1;
                let count = items.len();
                view! {
                    <div class="dispatch-group">
                        { crate::ui::session_group_head(state, &key, count) }
                        <For
                            each=move || items.clone()
                            key=|s: &SessionInfo| s.name.clone()
                            children=move |s: SessionInfo| {
                                crate::ui::session_card(state, s, true)
                            }
                        />
                    </div>
                }
            }
        />
    }
    .into_any()
}

/// The tree body: legend, recursive nodes, boundary footnotes.
fn hist_tree(state: AppState) -> AnyView {
    let tree = state.rewind_tree;
    let active = state.active_session;
    view! {
        { move || match (active.get(), tree.get()) {
            // No session selected yet (a fresh page load, before any card).
            (None, _) => view! { <div class="rw-empty">{ "select a session" }</div> }.into_any(),
            (_, None) => view! { <div class="rw-empty">{ "loading\u{2026}" }</div> }.into_any(),
            (_, Some(t)) if t.total_rounds == 0 => view! {
                <div class="rw-empty">
                    { "no rounds yet \u{2014} send a message to start the history." }
                </div>
            }.into_any(),
            (_, Some(t)) => {
                let sess = t.session.clone();
                let nodes = t.roots.clone();
                let abandoned = t.abandoned_rounds();
                let rewinds = t.rewinds.len();
                let boundaries = t.boundaries.clone();
                let boundaries_view: AnyView = if boundaries.is_empty() {
                    ().into_any()
                } else {
                    view! {
                        <div class="rw-boundaries">
                            <For
                                each=move || boundaries.clone()
                                key=|b: &crate::model::RewindBoundary| b.seq
                                children=move |b: crate::model::RewindBoundary| {
                                    view! {
                                        <div class="rw-boundary">
                                            { format!("compaction boundary at line {} \u{2014} rewinds older than it land at the boundary", b.seq) }
                                        </div>
                                    }
                                }
                            />
                        </div>
                    }
                    .into_any()
                };
                view! {
                    <div class="rw-legend">
                        <span class="rw-lg on">{ "active" }</span>
                        <span class="rw-lg off">{ format!("abandoned ({abandoned})") }</span>
                        <span class="rw-lg fork">{ format!("{rewinds} rewind{}", if rewinds == 1 { "" } else { "s" }) }</span>
                    </div>
                    <div class="rw-nodes">
                        { nodes_view(state, sess, nodes) }
                    </div>
                    { boundaries_view }
                }.into_any()
            }
        } }
    }
    .into_any()
}

/// One DOM level per **fork** (v0.5.67).
///
/// The list used to nest one `children` container per round, so a straight
/// chain of N rounds cost ~2N DOM levels and the wasm stack gave out
/// somewhere past ~32–80 rounds: `Webui` (117) and `alpha` (82) drew six
/// nodes and threw `RuntimeError: memory access out of bounds` (the app
/// survived, the tree never painted). A node with a **single** child now
/// continues as a sibling in the same container, so the recursion depth is
/// the number of *forks* along the path instead of the number of rounds —
/// and a pure chain draws as one flat list rather than a staircase.
fn nodes_view(state: AppState, sess: String, nodes: Vec<RewindNode>) -> Vec<AnyView> {
    let mut out: Vec<AnyView> = Vec::with_capacity(nodes.len());
    let mut queue: VecDeque<RewindNode> = nodes.into();
    while let Some(mut n) = queue.pop_front() {
        if n.children.len() == 1 {
            // the run continues: hoist the only child to be the next sibling
            if let Some(child) = n.children.pop() {
                queue.push_front(child);
            }
        }
        out.push(node_view(state, sess.clone(), n));
    }
    out
}

/// One round node + its fork children (via [`nodes_view`]). Abandoned
/// branches keep their dimmed styling; the current one carries the "here"
/// marker.
fn node_view(state: AppState, sess: String, node: RewindNode) -> AnyView {
    let seq = node.seq;
    let round = node.round;
    let summary = node.summary.clone();
    let events = node.events;
    let ts = timeutil::ts_full(&node.ts);
    let abandoned = node.state != "active";
    let current = node.current;
    let retracted = node.retracted;
    let kids = node.children.clone();
    let restore = node.restore.clone();
    let blocked = restore.blocked();
    let restore_label = restore.label();

    let sess_cls = sess.clone();
    let node_cls = move || {
        let mut c = String::from("rw-node");
        if abandoned {
            c.push_str(" abandoned");
        }
        if current {
            c.push_str(" current");
        }
        if !rewind_allowed(state, &sess_cls) {
            c.push_str(" locked");
        }
        if blocked {
            c.push_str(" unresumable");
        }
        c
    };
    let sess_title = sess.clone();
    let ts_title = ts.clone();
    let state_title = node.state.clone();
    let restore_title = restore_label.clone();
    let title = move || {
        let head = format!(
            "round {round} \u{00b7} {ts_title} \u{00b7} {events} event{} \u{00b7} {state_title}",
            if events == 1 { "" } else { "s" }
        );
        if !rewind_allowed(state, &sess_title) {
            format!("{head} \u{2014} rewind is disabled while the loop is running")
        } else if blocked {
            format!("{head} \u{2014} this point cannot be resumed: it would strand a tool call")
        } else if current {
            format!("{head} \u{2014} you are here \u{00b7} resumes from {restore_title}")
        } else {
            format!(
                "{head} \u{2014} resumes from {restore_title} \u{2014} click to rewind to this point"
            )
        }
    };
    let label = format!("round {round} \u{00b7} {summary}");
    let sess_click = sess.clone();
    let on_click = move |_| {
        if current || !rewind_allowed(state, &sess_click) {
            return;
        }
        request_rewind(state, seq, label.clone());
    };

    let badge: AnyView = if retracted {
        view! { <span class="rw-badge-retract">{ "retracted" }</span> }.into_any()
    } else {
        ().into_any()
    };
    let warn: AnyView = if blocked {
        view! { <span class="rw-badge-warn">{ "not resumable" }</span> }.into_any()
    } else {
        ().into_any()
    };
    let restore_text = restore_label.clone();

    let kids_view: AnyView = if kids.is_empty() {
        ().into_any()
    } else {
        let sess_kids = sess.clone();
        view! { <div class="rw-kids">{ nodes_view(state, sess_kids, kids) }</div> }.into_any()
    };

    view! {
        <div class=node_cls data-seq=seq.to_string() data-round=round.to_string()>
            <div class="rw-head" title=title on:click=on_click>
                <span class="rw-dot"></span>
                <span class="rw-round">{ format!("round {round}") }</span>
                <span class="rw-sum">{ summary.clone() }</span>
                { badge }
                { warn }
                <span class="rw-meta">
                    { format!("{events} event{}", if events == 1 { "" } else { "s" }) }
                </span>
                <span class="rw-restore">{ restore_text }</span>
                <span class="rw-time">{ ts.clone() }</span>
                <Show when=move || current fallback=|| ()>
                    <span class="rw-here">{ "here" }</span>
                </Show>
            </div>
            { kids_view }
        </div>
    }
    .into_any()
}

// ── Style B: the "flow" view (plan section 10, steps B4-B8) ───────
//
// The History view's second style. Style A (the recursive list) is
// untouched but no longer the default (v0.5.67); this one is the default
// and remembers the choice.
//
// The right-hand side splits 1 : 2 (D3): the **detail panel** on top (the
// selected round in full + the Rewind button, the only place a rewind can
// be triggered here) and the **scene** below — the longest chain drawn as
// a straight horizontal `. - . - .` line with every other branch forking
// into a lane above or below it (§10.2, computed server-side).
//
// Two rules from the user shape the interaction: clicking a node only
// **selects** it (no dialog, no write — `select_node`), and the current
// round is pre-selected so the panel is never empty (D5).

/// The `[ list | flow ]` switch in the History top bar. Labelled for the
/// user ("list" is the style they know), keyed for the code (`tree`).
fn hist_style_switch(state: AppState) -> AnyView {
    let view = state.rw_view;
    let opt = move |mode: &'static str, label: &'static str, hint: &'static str| {
        let on = {
            let view = view;
            move || view.get() == mode
        };
        view! {
            <button
                class="hs-opt"
                class:on=on
                data-view=mode
                title=hint
                on:click=move |_| set_view_mode(state, mode)
            >
                { label }
            </button>
        }
    };
    view! {
        <div id="hist-style" role="group" title="history style">
            { opt("tree", "list", "the round list \u{2014} click a round to rewind") }
            { opt("flow", "flow", "the flow graph \u{2014} click a round to inspect it") }
        </div>
    }
    .into_any()
}

/// Style B's right-hand side: the 1:2 split (detail over scene).
fn flow_view(state: AppState) -> AnyView {
    view! {
        <div id="rw-split">
            <div id="rw-detail">{ flow_detail(state) }</div>
            <div id="rw-flow">{ flow_scene(state) }</div>
        </div>
    }
    .into_any()
}

/// The detail panel: the selected round in full (B2's route), its
/// annotations, and the Rewind button (B8 — the only trigger in this
/// style, and disabled under the same guard as everywhere else).
fn flow_detail(state: AppState) -> AnyView {
    let selected = state.rw_selected;
    let detail = state.rw_detail;
    let tree = state.rewind_tree;

    view! {
        <Show
            when=move || selected.get().is_some()
            fallback=move || view! {
                <div class="rw-empty">{ "click a round below to inspect it" }</div>
            }
        >
            { move || {
                let Some(seq) = selected.get() else { return ().into_any() };
                // The panel's facts come from B2's per-round route; the flow
                // node is the fallback (and the only carrier of `restore`)
                // while the fetch is in flight.
                let d = detail.get().filter(|d| d.seq == seq);
                let node = tree
                    .get()
                    .and_then(|t| t.flow.nodes.iter().find(|n| n.seq == seq).cloned());
                let round = d
                    .as_ref()
                    .map(|d| d.round)
                    .or_else(|| node.as_ref().map(|n| n.round))
                    .unwrap_or_default();
                let events = d
                    .as_ref()
                    .map(|d| d.events)
                    .or_else(|| node.as_ref().map(|n| n.events))
                    .unwrap_or_default();
                let ts = d
                    .as_ref()
                    .map(|d| d.ts.clone())
                    .or_else(|| node.as_ref().map(|n| n.ts.clone()))
                    .unwrap_or_default();
                let abandoned = d
                    .as_ref()
                    .map(|d| d.state != "active")
                    .or_else(|| node.as_ref().map(|n| n.state != "active"))
                    .unwrap_or(false);
                let current = d
                    .as_ref()
                    .map(|d| d.current)
                    .or_else(|| node.as_ref().map(|n| n.current))
                    .unwrap_or(false);
                let retracted = d
                    .as_ref()
                    .map(|d| d.retracted)
                    .or_else(|| node.as_ref().map(|n| n.retracted))
                    .unwrap_or(false);
                let blocked = node.as_ref().is_some_and(|n| n.restore.blocked());
                let restore = node.as_ref().map(|n| n.restore.label()).unwrap_or_default();
                let text = d
                    .as_ref()
                    .map(|d| d.text.clone())
                    .or_else(|| node.as_ref().map(|n| n.summary.clone()))
                    .unwrap_or_default();
                let pending = d.is_none();
                let allowed = tree
                    .get()
                    .map(|t| rewind_allowed(state, &t.session))
                    .unwrap_or(false);
                let label = format!("round {round} \u{00b7} {}", summarize_label(&text));
                // the closure below owns one copy; the panel shows the other
                let restore_t = restore.clone();

                let badge = |cls: &'static str, text: &'static str| {
                    view! { <span class=cls>{ text }</span> }
                };
                let mut badges: Vec<AnyView> = Vec::new();
                if current {
                    badges.push(badge("rw3-badge here", "here").into_any());
                }
                if abandoned {
                    badges.push(badge("rw3-badge off", "abandoned").into_any());
                }
                if retracted {
                    badges.push(badge("rw3-badge retract", "retracted").into_any());
                }
                if blocked {
                    badges.push(badge("rw3-badge warn", "not resumable").into_any());
                }

                view! {
                    <div class="fd-head" data-pending=pending.to_string()>
                        <span class="fd-round">{ format!("round {round}") }</span>
                        <span class="fd-meta">
                            { format!(
                                "line {seq} \u{00b7} {} \u{00b7} {events} event{}",
                                timeutil::ts_full(&ts),
                                if events == 1 { "" } else { "s" },
                            ) }
                        </span>
                        <Show when=move || pending>
                            <span class="rw3-badge load">{ "loading\u{2026}" }</span>
                        </Show>
                        { badges }
                    </div>
                    <div class="fd-text">{ text }</div>
                    <div class="fd-foot">
                        <button
                            class="fd-rewind"
                            data-seq=seq.to_string()
                            disabled=move || !allowed || current || blocked
                            title=move || {
                                if !allowed {
                                    "rewind is disabled while the loop is running".to_string()
                                } else if current {
                                    "the session is already here".to_string()
                                } else if blocked {
                                    format!(
                                        "this point cannot be resumed: it would strand a tool call ({restore_t})"
                                    )
                                } else {
                                    format!("rewind to round {round} \u{00b7} {restore_t}")
                                }
                            }
                            on:click=move |_| {
                                if allowed && !current && !blocked {
                                    request_rewind(state, seq, label.clone());
                                }
                            }
                        >
                            { if current { "you are here" } else { "\u{21ba} Rewind to this point" } }
                        </button>
                        <span class="fd-restore">{ restore }</span>
                    </div>
                }
                .into_any()
            } }
        </Show>
    }
    .into_any()
}

/// The single value a scroll frame writes (B7): the offset the ribbons'
/// `--turn` calc reads. Never `scrollLeft` itself, so nothing reads layout
/// while scrolling, and `--halfpw`/`--denom`/`--bx` stay from the layout.
/// The pan-only fallback: move the scroller by `dy` (the browser cannot, see
/// above) and refresh the one value a scroll frame writes.
fn pan_only(el: &HtmlElement, dy: f64) {
    let max = (el.scroll_width() - el.client_width()).max(0) as f64;
    let want = (el.scroll_left() as f64 + dy).clamp(0.0, max);
    el.set_scroll_left(want as i32);
    mark_scroll(el);
}

fn mark_scroll(el: &HtmlElement) {
    let v = el.scroll_left() as f64 + turn();
    let _ = el.style().set_property("--rw-scroll", &format!("{v}"));
}

/// **v0.5.69 (round-3 defect 2)** → **v0.5.73 (D-snap-2)**: the wheel used to
/// come through here (`pan_and_turn`: pan as far as the track allows, keep the
/// rest as *turn*, so the cone kept moving when the trunk could not). Since the
/// focus carousel the vertical wheel is the **focus** control on a real fan,
/// and it measures its own delta against `--dpp` to decide which branch comes
/// up next ([`snap_step`]).
///
/// It still pans — as a **fallback** — when there is nothing to bring up (a fan
/// of 0 or 1 branch, i.e. every live session but `rewind`), and that has to go
/// through a proxy for the same reason it always did: the scroller is
/// `overflow-x` only, so the browser has nothing to scroll for a vertical
/// delta (measured: 260 → 260 with the event left alone).

/// Short label for the dialog's title line ("round 3 \u{00b7} first chars\u{2026}").
fn summarize_label(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 60 {
        flat
    } else {
        let s: String = flat.chars().take(60).collect();
        format!("{s}\u{2026}")
    }
}

/// The scene: the straight main line, the branches forking off it, one
/// clickable dot per round. The player: the server's logical `(x, lane)`
/// paid for in px here, with the column pitch left to CSS (it auto-fits
/// the panel down to a floor, D4).
fn flow_scene(state: AppState) -> AnyView {
    let tree = state.rewind_tree;
    // the scene draws the ring's alignment from the selection (D-orb-7)
    let selected = state.rw_selected;
    // **bug 3 (D-cone-12)**: the scene measures itself when it *mounts*. This
    // effect belongs to the scene's own lifetime, so it runs after the render
    // that produced this element — whatever brought the scene up (the History
    // opening, the plugin coming back, a style switch) and whoever else did or
    // did not change in the same tick.
    Effect::new(move || {
        if !arm_scene_probe_now() {
            // the tree is still loading: the scene mounts a moment later, and
            // that render's own tree effect measures and arms it
            gloo_timers::callback::Timeout::new(0, || {
                let _ = arm_scene_probe_now();
            })
            .forget();
        }
    });
    view! {
        { move || {
            let Some(t) = tree.get() else {
                return view! { <div class="rw-empty">{ "loading\u{2026}" } </div> }.into_any();
            };
            if t.flow.nodes.is_empty() {
                return view! {
                    <div class="rw-empty">
                        { "no rounds yet \u{2014} send a message to start the history." }
                    </div>
                }
                .into_any();
            }
            let f = t.flow.clone();
            let cells = f.cols.max(1).to_string();
            let lanes = f.lanes.max(1).to_string();
            // `orbit.step_deg` and `orbit.arc_deg` are no longer read: since
            // D-cone-8 the branch turns the whole circle, and since D-fan-1
            // (v0.5.72) the step is `360/k` of *each fan*, which only the
            // render knows (the server still emits both, unread).
            // Where each node sits, and whether it is on the live path: the
            // connectors are drawn from the parent's dot, and an edge is lit
            // by the child's state (D1 — the *path* is highlighted, so the
            // live context reads even when the current round is on a fork).
            let at: HashMap<u64, (u64, i32, bool)> = f
                .nodes
                .iter()
                .map(|n| (n.seq, (n.x, n.lane, n.state == "active")))
                .collect();

            // **v0.5.68**: which fin each round belongs to, server-grouped
            // (`branch.seqs` — a nested fork shares its parent's lane, so the
            // lane cannot group them), and which fin sits at the front at
            // rest. D-orb-7: the selected round's branch, else the current
            // round's, else the first — so the picture answers "where am I"
            // without reading a word.
            let fin_of: HashMap<u64, u32> = f
                .branches
                .iter()
                .filter_map(|b| b.fin.map(|i| (b, i)))
                .flat_map(|(b, i)| b.seqs.iter().map(move |s| (*s, i)))
                .collect();

            // The trunk: the main-line runs. They never turn — the branches
            // orbit around *them* (D-orb-3, user decision) — so the trunk's
            // rect is bit-identical before and after a scroll.
            let trunk_edges: Vec<AnyView> = f
                .edges
                .iter()
                .filter(|e| e.main)
                .map(|e| {
                    let (fx, _, _) = at.get(&e.from).copied().unwrap_or((0, 0, true));
                    let lit = at.get(&e.to).map(|t| t.2).unwrap_or(true);
                    view! {
                        <div
                            class=format!(
                                "rw3-seg main {}",
                                if lit { "active" } else { "abandoned" },
                            )
                            data-from=e.from.to_string()
                            data-to=e.to.to_string()
                            style=format!("--x:{fx}; --lane:0")
                        ></div>
                    }
                    .into_any()
                })
                .collect();

            let by_seq: HashMap<u64, FlowNode> =
                f.nodes.iter().map(|n| (n.seq, n.clone())).collect();

            // ── the branches are a *forest*, not a ring of siblings ─────            // v0.5.70 / D-cone-6/7: a branch leaves its parent's **bead**. A
            // trunk-parented branch hinges on the axis at its parent's column;
            // a branch whose parent is itself on a branch is rendered *inside*
            // its parent's container, so the parent's turn is already in force
            // and the child's own angle reads as a small fan (±1 slot)
            // relative to its parent. The relation comes from the reduced edge
            // (a branch root's parent), never from the lane — a nested fork
            // shares its parent's lane (v0.5.68).
            let by_fin: HashMap<u32, &FlowBranch> = f
                .branches
                .iter()
                .filter_map(|b| b.fin.map(|i| (i, b)))
                .collect();
            let parent_of: HashMap<u64, u64> = f
                .edges
                .iter()
                .filter(|e| !e.main)
                .map(|e| (e.to, e.from))
                .collect();
            let owner: HashMap<u32, Option<u32>> = by_fin
                .iter()
                .map(|(i, b)| {
                    let o = parent_of
                        .get(&b.root)
                        .and_then(|p| fin_of.get(p).copied());
                    (*i, o)
                })
                .collect();
            let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
            let mut roots: Vec<u32> = Vec::new();
            for (&i, o) in owner.iter() {
                match o {
                    Some(p) => kids.entry(*p).or_default().push(i),
                    None => roots.push(i),
                }
            }
            roots.sort_unstable();
            for v in kids.values_mut() {
                v.sort_unstable();
            }
            // The topmost trunk-parented ancestor of each fin: the one
            // `--align` turns to the front (D-orb-7) — which brings a nested
            // fan along with it, since a child's turn composes with its
            // parent's.
            let mut top: HashMap<u32, u32> = HashMap::new();
            for &i in &roots {
                top.insert(i, i);
            }
            for _ in 0..by_fin.len() {
                let known: Vec<(u32, u32)> = kids
                    .iter()
                    .filter_map(|(p, cs)| top.get(p).map(|t| (*t, cs.clone())))
                    .flat_map(|(t, cs)| cs.into_iter().map(move |c| (c, t)))
                    .collect();
                if known.is_empty() {
                    break;
                }
                for (c, t) in known {
                    top.insert(c, t);
                }
            }
            let align = selected
                .get()
                .and_then(|s| fin_of.get(&s).copied())
                .or_else(|| t.current_seq.and_then(|s| fin_of.get(&s).copied()))
                .and_then(|i| top.get(&i).copied())
                .unwrap_or_else(|| roots.first().copied().unwrap_or(0));
            // D-fan-1/2 (user decision, v0.5.72): the angle a branch gets is
            // `360/k` of **its own fan**, and the rank it holds there — the
            // server's `fin` is global over the session, and the root fan of
            // the live fixture holds {0, 2, 3}, not {0, 1, 2}.
            //
            // **v0.5.73 (plan §15, D-snap)**: the root fan's angles are now
            // *focus-relative*: the branch the wheel has walked to sits at 0
            // (on top) and every other branch is spread over the lower arc. The
            // focus is a view state of its own (`rw_focus`), never the
            // selection — the detail panel must not churn while browsing
            // (D-snap-7).
            let root_k = roots.len();
            let focus_rank = if root_k == 0 {
                0
            } else {
                state.rw_focus.get().rem_euclid(root_k as i32)
            };
            let focus_fin = roots.get(focus_rank as usize).copied().unwrap_or(align);
            let root_step = fan_step(root_k, false);
            let root_theta = |i: u32| {
                fan_root_theta(
                    roots.iter().position(|&x| x == i).unwrap_or(0) as i32,
                    focus_rank,
                    root_k,
                )
            };

            let cone = Cone {
                state,
                at: &at,
                by_seq: &by_seq,
                by_fin: &by_fin,
                parent_of: &parent_of,
                kids: &kids,
                focus_fin,
            };
            let fins: Vec<AnyView> = roots
                .iter()
                .map(|&i| {
                    // the `--align` rotation and the angles are the render's
                    // now (D-fan-1): a root's `--rdeg` is its own angle, which
                    // every container below it re-reads as its absolute turn.
                    let theta = root_theta(i);
                    let rank = roots.iter().position(|&x| x == i).unwrap_or(0) as u32;
                    cone.branch(i, 0, 0, rank, root_step, theta, theta, 0.0, 1.0, 0)
                })
                .collect();

            let trunk_nodes: Vec<AnyView> = f
                .nodes
                .iter()
                .filter(|n| !fin_of.contains_key(&n.seq))
                .map(|n| flow_node_button(state, n.clone(), 0))
                .collect();

            // Drag the empty space to pan (D4/D7): the wheel drives the same
            // scroll offset natively (an x-only scroller takes the vertical
            // delta), the dots are excluded so a click is never eaten.
            let drag = StoredValue::new((false, 0.0, 0.0, 0.0));
            let el_of = |ev: &PointerEvent| -> Option<HtmlElement> {
                ev.current_target()?.dyn_into::<HtmlElement>().ok()
            };
            let on_down = move |ev: PointerEvent| {
                // a new gesture retargets any flight still in the air
                cancel_snap();
                let Some(el) = el_of(&ev) else { return };
                // a drag starts after some layout, possibly: re-measure
                sync_scene_metrics();
                let on_node = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .and_then(|t| t.closest(".rw3-node").ok().flatten())
                    .is_some();
                if on_node {
                    return;
                }
                drag.set_value((
                    true,
                    ev.client_x() as f64,
                    el.scroll_left() as f64,
                    turn(),
                ));
                let _ = el.set_pointer_capture(ev.pointer_id());
                let _ = el.class_list().add_1("dragging");
            };
            let on_move = move |ev: PointerEvent| {
                let (down, x0, sl, t0) = drag.get_value();
                if !down {
                    return;
                }
                let Some(el) = el_of(&ev) else { return };
                let dx = ev.client_x() as f64 - x0;
                // The pointer's *absolute* wish (the drag is anchored at
                // the press), and the part of it the track refuses becomes
                // the turn — continuous, so dragging past the end keeps
                // turning the ring instead of stopping dead.
                let want = sl - dx;
                el.set_scroll_left(want as i32);
                let got = el.scroll_left() as f64;
                set_turn(t0 + (want - got));
                mark_scroll(&el);
            };
            let on_up = move |ev: PointerEvent| {
                let (down, x0, _, _) = drag.get_value();
                if let Some(el) = el_of(&ev) {
                    let _ = el.release_pointer_capture(ev.pointer_id());
                    let _ = el.class_list().remove_1("dragging");
                    // **D-snap-3 (plan §15.4)**: a drag settles onto the next
                    // detent in the direction it travelled — the same rule as
                    // the wheel, so the gesture is never a no-op (v0.5.69). A
                    // press that did not move (a click on empty space) stays a
                    // no-op, exactly as before.
                    let moved = ev.client_x() as f64 - x0;
                    // a fan of 0 or 1 branch has nothing to bring up — the
                    // drag keeps panning it, as before (v0.5.69's rule is about
                    // *gestures*, and the pan **is** the effect there)
                    if root_k >= 2 && down && moved.abs() >= 2.0 {
                        let dir = if moved < 0.0 { 1 } else { -1 };
                        snap_step(state, &el, dir, root_k, focus_rank);
                    }
                }
                drag.set_value((false, 0.0, 0.0, 0.0));
            };
            // Any scroll (this is also the fallback for a scrollbar drag we
            // never see, a keyboard scroll, or a programmatic `scrollLeft`
            // — the probe's H7 found the stale-var case) refreshes the same
            // single value.
            let on_scroll = move |ev: web_sys::Event| {
                if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<HtmlElement>().ok())
                {
                    mark_scroll(&el);
                }
            };
            // **v0.5.73 (plan §15, D-snap-2)**: the plain vertical wheel is now
            // the **focus** control — one notch brings the next branch up and
            // snaps it (the user's ask), instead of panning the line. The pan
            // keeps every other home: the drag, a horizontal delta (which stays
            // the browser's own x-scroll, D9) and the scrollbar. On the live
            // sessions nothing is lost: their track fits the panel exactly, so
            // the wheel could never pan there anyway.
            //
            // **The fallback (measured, 2026-10-05)**: on a fan of **0 or 1
            // branch** there is nothing to bring up (a lone branch is always
            // the focused one under this law — D-cone-11 is superseded), so the
            // wheel is left to the browser and pans the line natively, exactly
            // as it did before the carousel. That is what keeps v0.5.69's rule
            // true on the branchless live sessions (the check that caught this
            // was flow_style_b_probe's F4, on the 117-round one).
            let on_wheel = move |ev: web_sys::WheelEvent| {
                let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<HtmlElement>().ok())
                else {
                    return;
                };
                let dy = ev.delta_y()
                    * match ev.delta_mode() {
                        1 => 16.0,
                        2 => el.client_height() as f64,
                        _ => 1.0,
                    };
                if dy.abs() < 0.5 {
                    return;
                }
                if root_k >= 2 {
                    ev.prevent_default();
                    snap_step(state, &el, if dy > 0.0 { 1 } else { -1 }, root_k, focus_rank);
                } else {
                    // nothing to focus: pan, exactly as v0.5.69 did
                    ev.prevent_default();
                    pan_only(&el, dy);
                }
            };

            view! {
                <div
                    id="rw-flow-scroll"
                    on:pointerdown=on_down
                    on:pointermove=on_move
                    on:pointerup=on_up
                    on:pointercancel=on_up
                    on:wheel=on_wheel
                    on:scroll=on_scroll
                >
                    <div
                        id="rw-flow-track"
                        style=format!(
                            "--cols:{cells}; --lanes:{lanes}",
                        )
                    >
                        <div class="rw3-grid">
                            { trunk_edges }
                            { trunk_nodes }
                            // D-cone-11 (user decision, v0.5.71): every branch
                            // turns the same way — a lone one orbits the trunk
                            // like the rest, so the ring carries no `solo`
                            // special case any more.
                            <div class="rw-orbit">{ fins }</div>
                        </div>
                        <div class="rw3-sheen"></div>
                    </div>
                </div>
            }
            .into_any()
        } }
    }
    .into_any()
}

/// One straight run of a branch's **spine** (plan §13, D-cone-4): from the
/// bead `d0` steps out of the parent, `n` beads / `dx` columns long.
/// `hypot()` and `atan2()` give the bar its length and angle *in the
/// branch's own plane*, so the scene bends nowhere and the layout writes only
/// the slope (`--q`) — one number for the whole cone.
fn spine_bar(d0: usize, n: usize, dx: u64, live: bool) -> AnyView {
    view! {
        <div
            class=format!(
                "rw-br-line {}",
                if live { "active" } else { "abandoned" },
            )
            data-from=d0.to_string()
            data-to=(d0 + n).to_string()
            style=format!("--d0:{d0}; --n:{n}; --dx:{dx}")
        ></div>
    }
    .into_any()
}

/// How close to `±90°` a branch may come before the fan is nudged: `cos a` is
/// what is left of the ray's projected length, and at 5° off the camera's axis
/// there is almost nothing of it (the beads land on the trunk row — measured
/// at the N=4 spacing: an edge-on bead sat 1.7px from a trunk bead).
const FAN_EDGE_ON: f64 = 0.087;

/// The angle of a **root** fan's branch at `rank`, with `focus` the rank that
/// is **on top of the cone** (plan §15, D-snap-1, user decision 2026-10-05):
/// *"自动吸附对应的分支到圆锥面正上方的位置，此时不管其余有多少分支，全部都位于圆锥面的
/// 下半部分"* — the focused branch at 0, **every** other branch in the lower
/// half, whatever `k` is.
///
/// The law is *relative to the focus* (it re-hangs the fan for each of the `k`
/// detents), so "all of them below" is achievable for any fan size — a *rigid*
/// rotation of one fixed shape could only do it for `k ≤ 3` (every forward gap
/// would have to exceed 90° while the gaps sum to 360°). What a focus-relative
/// law costs is that the fan's shape changes as the focus walks; it stays
/// *continuous* exactly for the fans this law handles specially:
///
/// * `k = 1` — nothing to place;
/// * `k = 2` — the other branch sits straight down;
/// * `k ≥ 3` — the others are spread evenly over the lower arc, which starts
///   `FAN_ARC_INSET` below the horizontal on one side and ends as far above it
///   on the other (for `k = 3` that is exactly v0.5.72's 120°/240°).
///
/// The slot order is the walk's own: `s = 1` is the branch a positive wheel
/// brings up first, and the angles are assigned so that increasing the phase
/// visits `s = 1, 2, … , k-1, 0` — the fan's rank order, wrapping.
///
/// One honest cost, for fans of **4 or more**: the law re-hangs the fan for the
/// new focus, and that shape is not a rigid rotation of the old one, so the
/// branches *relocate* (by up to one arc gap) at the instant the focus commits.
/// No live session has such a fan (the `rewind` fixture's root fan is 3); the
/// alternative — the rigid even fan — is the one that would put a branch
/// exactly edge-on at rest, which is the defect this round exists to remove.
/// It is the plan's open question D-snap-6.
fn fan_root_theta(rank: i32, focus: i32, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let k = k as i32;
    let mut s = (rank - focus) % k;
    if s < 0 {
        s += k;
    }
    if s == 0 {
        return 0.0;
    }
    if k == 2 {
        return 180.0;
    }
    let step = (180.0 - 2.0 * FAN_ARC_INSET) / (k as f64 - 2.0);
    wrap180(90.0 + FAN_ARC_INSET + (k - 1 - s) as f64 * step)
}

/// The angular spacing of a fan of `k` branches (plan §13.9, D-fan-1): the
/// branches of **one parent** share the full circle evenly — `360/k` apart —
/// instead of the fixed 30° step, so a fan never crowds. A nested fan caps its
/// spread at 120° (D-fan-4). Written on the container as `--step` for the
/// stylesheet's benefit; the render is the only thing that reads it.
fn fan_step(k: usize, nested: bool) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let step = 360.0 / k as f64;
    if nested {
        step.min(120.0)
    } else {
        step
    }
}

/// The angle of a **nested** fan's branch at `rank` (D-fan-4): the fan is
/// centred on 180° — opposite the parent's own ray, so no child ever runs along
/// its parent's line (D-cone-7 measured a +30° child hiding 4.5px behind its
/// parent's next bead) — and its spread is capped at 120°. A lone child is
/// therefore straight below its parent.
///
/// The singular ±90 can reappear for some fan sizes (6, 10, 14, … children) and
/// is nudged by a **quarter** step: half a step would move a child onto the
/// parent's own ray, and a quarter step is off both (180/h even in every such
/// case, so no nudged rank reaches ±90 or 0 either).
fn fan_nested_theta(rank: usize, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let step = fan_step(k, true);
    let at = |j: usize, shift: f64| {
        180.0 + (j as f64 - (k as f64 - 1.0) / 2.0) * step + shift
    };
    let nudge = if (0..k).any(|j| at(j, 0.0).to_radians().cos().abs() < FAN_EDGE_ON) {
        step / 4.0
    } else {
        0.0
    };
    at(rank, nudge)
}

/// The cone renderer (plan §13). One method, recursing down the branch
/// *forest*, so a fork off a branch knows its parent's bead and every branch
/// keeps one straight ray leaving it.
struct Cone<'a> {
    state: AppState,
    at: &'a HashMap<u64, (u64, i32, bool)>,
    by_seq: &'a HashMap<u64, FlowNode>,
    by_fin: &'a HashMap<u32, &'a FlowBranch>,
    parent_of: &'a HashMap<u64, u64>,
    kids: &'a HashMap<u32, Vec<u32>>,
    /// **v0.5.73 (plan §15)**: the root branch that is on top. Only the root
    /// container carries `data-focus`/`.unfocused` — a nested fan lives inside
    /// its parent and is *not* part of the root fan's walk (D-snap-8), so it
    /// neither shrinks nor reports a focus of its own.
    focus_fin: u32,
}

/// Style B's flat projection shrinks a branch's outward step by this factor
/// (plan §13, D9/L10). It is the same law the stylesheet's `--kroot` uses, so
/// a nested bead can counter-scale by its chain's product and stay round.
fn flat_k(deg: f64) -> f64 {
    let r = deg.to_radians();
    r.cos() - 0.12 * r.sin()
}

impl Cone<'_> {
    /// One branch: a container sitting at (or inside) its parent's bead, one
    /// straight spine, its rounds, and the branches that fork off it.
    ///
    /// `outer` is the hinge of the container this one lives in (0 when it
    /// lives on the trunk) and `po` its parent bead's step inside that
    /// container. `rank` is this branch's rank inside **its own fan** (D-fan-2:
    /// the server's `fin` is global over the session, a fan only holds a
    /// subset, so the rank is what the angles are made of), `step` that fan's
    /// spacing, and `theta` the angle the render already worked out. `rdeg` is
    /// the **topmost trunk-parented ancestor's** angle and `sum` the static part
    /// of the nesting chain — together they give the plane's *absolute* angle,
    /// which a bead needs to counter-rotate (the CSS cannot add an ancestor's
    /// own variable to its own without a cycle).
    #[allow(clippy::too_many_arguments)]
    fn branch(
        &self,
        i: u32,
        outer: u64,
        po: u32,
        rank: u32,
        step: f64,
        theta: f64,
        rdeg: f64,
        sum: f64,
        kup: f64,
        depth: u32,
    ) -> AnyView {
        let Some(b) = self.by_fin.get(&i).copied() else {
            return view! { <div></div> }.into_any();
        };
        // this container's own static flat factor (1 at the root: the root's
        // turn carries the phase, so the stylesheet reads that one from
        // `--root_a`). The children inherit it through `--kup`.
        // D-cone-8: no clamp any more — a nested fan's angle is a real angle
        // and on a cone ±120° is a place like any other, instead of a park that
        // would land two children on top of each other.
        let kown = if depth == 0 { 1.0 } else { flat_k(theta) };
        let hinge = b.hinge_x;
        let seqs = &b.seqs;
        let n = seqs.len();
        let last_x = seqs
            .last()
            .and_then(|s| self.at.get(s))
            .map(|t| t.0)
            .unwrap_or(hinge);
        let dx = last_x.saturating_sub(hinge).max(1);
        // How far the live path runs into this branch: the spine is one
        // straight bar per run, so a branch that is lit where it leaves its
        // parent but buried further along still shows its dead tail as dead.
        let live = seqs
            .iter()
            .take_while(|s| self.at.get(s).map(|t| t.2).unwrap_or(false))
            .count();
        // **v0.5.73**: how many extra steps out a **nested** branch starts —
        // see the bead comment below. Two is the measured minimum that clears
        // *every* neighbour at every detent (the child's bead against the trunk
        // row, against its parent's other beads, and against the sibling
        // branch): modelled on the live fixture at 44/29/29px, where one step
        // left it 8.7px from a sibling's bead.
        const NESTED_SHIFT: u32 = 2;
        let shift: u32 = if depth > 0 { NESTED_SHIFT } else { 0 };
        // `--n`/`data-n` are the branch's *reach* in steps — the last bead's
        // `out` — not its bead count: the fit reads `data-n` off the DOM to size
        // the slope, and the spine must end on the last bead.
        let n_eff = n + shift as usize;
        let spines: Vec<AnyView> = if live == 0 || live >= n {
            vec![spine_bar(0, n_eff, dx, live > 0)]
        } else {
            vec![
                spine_bar(0, live + shift as usize, dx.min(live as u64).max(1), true),
                spine_bar(
                    live + shift as usize,
                    n_eff - live - shift as usize,
                    dx.saturating_sub(live as u64).max(1),
                    false,
                ),
            ]
        };
        // the beads: the k-th round of the branch rides `k+1` steps out, so
        // the first one leaves the parent rather than sitting on it.
        //
        // **v0.5.73 (plan §14.1's K1, done here instead of by a solver)**: a
        // **nested** branch starts one step further out. K1 is the exact
        // coincidence of a nested branch whose container offset `po` equals a
        // bead's `out` (a child of its parent's *first* bead — the live
        // fixture): the radial term `(po − out)·q` and the depth term
        // `−(po − out)·q·sin a` both cancel, so the child's first bead lands on
        // the trunk row at **every** phase (measured 0.0px, 24/24 phases) and
        // its dot permanently covers the trunk's round. `po + 1` cannot cancel
        // for any phase, so the collision is gone by construction — and the
        // spine is lengthened by the same step (`--n` below) so it still starts
        // at the parent's bead and ends on its last bead.
        let beads: Vec<AnyView> = seqs
            .iter()
            .enumerate()
            .filter_map(|(k, s)| {
                self.by_seq
                    .get(s)
                    .map(|nd| (k as u32 + 1 + shift, nd.clone()))
            })
            .map(|(out, nd)| flow_node_button(self.state, nd, out))
            .collect();
        // the branches that fork off this one, as nested containers
        let child_views: Vec<AnyView> = self
            .kids
            .get(&i)
            .map(|cs| {
                let ck = cs.len();
                cs.iter()
                    .enumerate()
                    .map(|(j, &c)| {
                        // the child's parent round is *this* branch's bead at
                        // that column — where the child's spine must start
                        let p = self
                            .by_fin
                            .get(&c)
                            .and_then(|cb| self.parent_of.get(&cb.root))
                            .copied()
                            .unwrap_or(b.root);
                        let po = seqs.iter().position(|s| *s == p).map(|q| q as u32 + 1);
                        // **D-fan-4 (user decision, v0.5.72)**: a nested fan is
                        // spread evenly over the full circle too, but centred on
                        // 180 — opposite this branch's own ray — so a lone child
                        // sits straight below its parent and never on its
                        // parent's line. Its spread is capped at 120°.
                        let ctheta = fan_nested_theta(j, ck);
                        // the child's `--kup` is every *ancestor* level's factor
                        // — this container's own included, the child's own
                        // excluded (the child adds that itself); `sum` likewise
                        // gains the child's own angle (the beads inside it need
                        // the whole chain)
                        self.branch(
                            c,
                            hinge,
                            po.unwrap_or(0_u32),
                            j as u32,
                            fan_step(ck, true),
                            ctheta,
                            rdeg,
                            sum + ctheta,
                            kup * kown,
                            depth + 1,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // the container's own static flat factor (1 at the root: its turn
        // carries the phase, so the stylesheet reads it from `--root_a`)
        //
        // **v0.5.73 (plan §15)**: only the root containers take part in the
        // focus carousel — `data-focus` and `.unfocused` are what the fit reads
        // and what the stylesheet scales (D-snap-5/8).
        let is_root = depth == 0;
        let focused = is_root && i == self.focus_fin;
        let cls = if is_root && !focused {
            "rw-branch unfocused"
        } else {
            "rw-branch"
        };
        view! {
            <div
                class=cls
                data-fin=i.to_string()
                data-root=b.root.to_string()
                data-hinge=hinge.to_string()
                data-n=n_eff.to_string()
                data-lane=b.lane.to_string()
                data-depth=depth.to_string()
                data-focus=if focused { "1" } else { "0" }
                data-th=format!("{theta:.4}")
                style=format!(
                    "--slot:{rank}; --step:{step:.4}; --th:{theta:.4}; \
                     --hinge:{hinge}; --ph:{outer}; --po:{po}; --n:{n}; \
                     --dx:{dx}; --rdeg:{rdeg:.4}; --sum:{sum:.4}; \
                     --kup:{kup:.5}; --kown:{kown:.5}",
                )
            >
                { spines } { beads } { child_views }
            </div>
        }
        .into_any()
    }
}

/// One round: a dot on the line (or on its branch). A click only **selects**
/// it — the Rewind button in the panel is the only trigger in this style.
fn flow_node_button(state: AppState, n: FlowNode, out: u32) -> AnyView {
    let seq = n.seq;
    let FlowNode {
        x,
        lane,
        main,
        round,
        current,
        retracted,
        summary,
        ..
    } = n;
    let abandoned = n.state != "active";
    let blocked = n.restore.blocked();
    let selected = state.rw_selected;
    let title = format!(
        "round {round} \u{00b7} {summary}{}",
        if current {
            " \u{2014} you are here"
        } else if abandoned {
            " \u{2014} abandoned"
        } else {
            ""
        }
    );
    let cls = move || {
        let mut c = String::from("rw3-node");
        c.push_str(if main { " main" } else { " off" });
        if abandoned {
            c.push_str(" abandoned");
        }
        if current {
            c.push_str(" current");
        }
        if retracted {
            c.push_str(" retracted");
        }
        if blocked {
            c.push_str(" blocked");
        }
        if selected.get() == Some(seq) {
            c.push_str(" selected");
        }
        c
    };
    view! {
        <button
            class=cls
            data-seq=seq.to_string()
            data-x=x.to_string()
            data-lane=lane.to_string()
            data-main=main.to_string()
            data-current=current.to_string()
            data-out=out.to_string()
            title=title
            style=format!("--x:{x}; --lane:{lane}; --out:{out}")
            on:click=move |_| select_node(state, seq)
        >
            <span class="rw3-dot"></span>
            <span class="rw3-round">{ round.to_string() }</span>
        </button>
    }
    .into_any()
}

// ── the confirm dialog ────────────────────────────────────────────

/// Sidebar/card confirm step: "Rewind to this point?" → Cancel /
/// Rewind. On confirm, writes `mode:"on"` for the pending target and bumps
/// `rewind_gen` so the tree refetches even if the WS frame is missed.
pub fn rewind_confirm_dialog(state: AppState) -> AnyView {
    let open = state.rewind_pending;
    let err = RwSignal::new(Option::<String>::None);
    // The post-hoc notice: the marker landed but the kernel's
    // projection will drop it (the response verdict, D-C).
    let warn = RwSignal::new(Option::<String>::None);

    let close = move || {
        crate::ui::after_dispatch(move || {
            open.set(None);
            err.set(None);
            warn.set(None);
        });
    };

    // R3: what this node restores (the tree's annotation), and whether
    // the pick must be refused (the kernel would ignore the marker).
    let restore = move || {
        let seq = open.get().map(|t| t.seq)?;
        state.rewind_tree.get()?.restore_at(seq)
    };
    let blocked = move || restore().map(|r| r.blocked()).unwrap_or(false);
    let restore_note = move || match restore() {
        Some(r) if r.blocked() => match r {
            Restore::Unresumable { missing } => format!(
                "This point cannot be resumed: the context it restores would leave tool call `{missing}` without its result, so the loop would ignore the rewind. Pick the round's first message instead."
            ),
            _ => String::new(),
        },
        Some(r) => format!("The context resumes from {}.", r.label()),
        None => String::new(),
    };

    let confirm = move |_| {
        let Some(t) = open.get_untracked() else {
            return;
        };
        let Some(sess) = state.active_session.get_untracked() else {
            err.set(Some("no active session".to_string()));
            return;
        };
        if !rewind_allowed(state, &sess) {
            err.set(Some("the loop is running".to_string()));
            return;
        }
        // The pre-check in the tree: a pick the kernel would ignore is
        // refused before the write (plan 11.1/11.2).
        if blocked() {
            return;
        }
        warn.set(None);
        spawn_local(async move {
            match api::post_rewind(&sess, t.seq, "on").await {
                Ok(verdict) => {
                    state.rewind_gen.update(|g| *g += 1);
                    match verdict.notice() {
                        // The marker landed but the kernel's projection
                        // drops it: say so and stay open (D-C). The
                        // user can pick another round.
                        Some(n) => crate::ui::after_dispatch(move || {
                            warn.set(Some(n));
                            err.set(None);
                        }),
                        None => crate::ui::after_dispatch(move || {
                            open.set(None);
                            err.set(None);
                            warn.set(None);
                        }),
                    }
                }
                Err(e) => err.set(Some(e)),
            }
        });
    };

    view! {
        <Show when=move || open.get().is_some() fallback=|| ()>
            <div id="rw-backdrop" on:click=move |_| close()>
                <div id="rw-dialog" on:click=move |e: MouseEvent| e.stop_propagation()>
                    <div class="rw-title">{ "Rewind to this point?" }</div>
                    { move || open.get().map(|t| view! {
                        <div class="rw-body">
                            <div class="rw-target">{ t.label }</div>
                            <div class="rw-note">
                                { "The active conversation resumes from here. Everything after it moves to an abandoned branch \u{2014} it stays in the history tree, and you can rewind back to it later." }
                            </div>
                        </div>
                    }) }
                    { move || {
                        let note = restore_note();
                        (!note.is_empty()).then(|| view! {
                            <div class="rw-restore-note" class:rw-block=blocked()>{ note }</div>
                        })
                    } }
                    { move || err.get().map(|e| view! { <div class="rw-err">{ e }</div> }) }
                    { move || warn.get().map(|w| view! { <div class="rw-warn">{ w }</div> }) }
                    <div class="rw-actions">
                        <button class="ns-btn ns-cancel" on:click=move |_| close()>{ "Cancel" }</button>
                        <button
                            class="ns-btn ns-danger"
                            disabled=move || blocked() || match state.active_session.get() {
                                Some(s) => !rewind_allowed(state, &s),
                                None => true,
                            }
                            on:click=confirm
                        >
                            { "Rewind" }
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
    .into_any()
}

/// The transcript quick button (`\u{27f2}`) on a user-message card: opens the
/// same confirm dialog for `card_seq` (the card's own log line —
/// `hist_oldest_line + card index`, the same 1-based non-empty-line counting
/// the kernel's `seq` uses). Disabled while the loop runs and on the current
/// tail (there is nothing to abandon there).
pub fn quick_rewind_button(state: AppState, card_seq: u64, excerpt: String) -> AnyView {
    let at_tail = move || state.rewind_tree.get().and_then(|t| t.current_seq) == Some(card_seq);
    let locked = move || {
        state
            .active_session
            .get()
            .map(|s| !rewind_allowed(state, &s))
            .unwrap_or(true)
    };
    // R1/R3: the tree's annotation for this card's line, when the tree
    // has it (a card outside the tree — an edited log — stays pickable
    // and the dialog's post-hoc notice covers it).
    let restore = move || state.rewind_tree.get()?.restore_at(card_seq);
    let blocked = move || restore().map(|r| r.blocked()).unwrap_or(false);
    let title = move || {
        if locked() {
            "rewind is disabled while the loop is running".to_string()
        } else if at_tail() {
            "you are already here".to_string()
        } else if blocked() {
            "this point cannot be resumed: it would strand a tool call".to_string()
        } else {
            "rewind to this message".to_string()
        }
    };
    let on_click = move |ev: MouseEvent| {
        // The card itself is clickable (compact/unfold in the pile engine).
        ev.stop_propagation();
        let Some(s) = state.active_session.get_untracked() else {
            return;
        };
        if locked() || at_tail() || !rewind_allowed(state, &s) {
            return;
        }
        request_rewind(state, card_seq, excerpt.clone());
    };
    view! {
        <div class="ev-quick-row">
            <button
                class="ev-quickbtn"
                title=title
                disabled=move || locked() || at_tail()
                on:click=on_click
            >
                { "\u{27f2}" }
            </button>
        </div>
    }
    .into_any()
}

// ── the sidebar plugin panel (registered in plugins.rs) ───────────

/// Compact panel: what the plugin is, the active path, and the full-view
/// entry. Rules 3/6 of `docs/plugin-authoring-rules.md` (one scroller, pinned
/// footer, fetch on mount/session change through the shared tree effect).
pub fn rewind_plugin_view(state: AppState) -> AnyView {
    let tree = state.rewind_tree;
    let sess = state.active_session;
    let layout = state.layout_mode;

    view! {
        <div id="rewind-body">
            { move || match (sess.get(), tree.get()) {
                (None, _) => view! {
                    <div class="plugin-empty">{ "no session selected" }</div>
                }.into_any(),
                (_, None) => view! {
                    <div class="plugin-empty">{ "loading\u{2026}" }</div>
                }.into_any(),
                (_, Some(t)) if t.total_rounds == 0 => view! {
                    <div class="plugin-empty">{ "no rounds yet" }</div>
                }.into_any(),
                (_, Some(t)) => {
                    let path = t.active_path();
                    let n = t.total_rounds;
                    let a = t.active_rounds();
                    let ab = t.abandoned_rounds();
                    let notice = t.tail_ignored.as_ref().map(|m| m.notice());
                    view! {
                        { notice.map(|msg| view! {
                            <div class="rewind-notice">{ msg }</div>
                        }) }
                        <div class="rewind-meta">
                            { format!("{n} rounds \u{00b7} {a} active \u{00b7} {ab} abandoned") }
                        </div>
                        <div class="rewind-list">
                            <For
                                each=move || path.clone()
                                key=|x: &RewindNode| x.seq
                                children=move |x: RewindNode| active_row(state, x)
                            />
                        </div>
                    }.into_any()
                }
            } }
        </div>
        <div class="rewind-actions">
            <button
                id="rewind-open-hist"
                title="open the full history tree"
                on:click=move |_| {
                    crate::ui::set_layout_mode("full");
                    layout.set("full".to_string());
                }
            >
                { "open History view" }
            </button>
        </div>
    }
    .into_any()
}

/// One active-path row in the sidebar panel: click → the same confirm dialog.
fn active_row(state: AppState, n: RewindNode) -> AnyView {
    let seq = n.seq;
    let round = n.round;
    let summary = n.summary.clone();
    let current = n.current;
    let label = node_label(&n);
    let row_cls = move || {
        let locked = state
            .active_session
            .get()
            .map(|s| !rewind_allowed(state, &s))
            .unwrap_or(true);
        let mut c = String::from("rewind-row");
        if current {
            c.push_str(" current");
        }
        if locked {
            c.push_str(" locked");
        }
        c
    };
    let on_click = move |_| {
        let Some(s) = state.active_session.get_untracked() else {
            return;
        };
        if current || !rewind_allowed(state, &s) {
            return;
        }
        request_rewind(state, seq, label.clone());
    };
    view! {
        <button class=row_cls on:click=on_click>
            <span class="rewind-row-round">{ format!("r{round}") }</span>
            <span class="rewind-row-sum">{ summary }</span>
            <Show when=move || current fallback=|| ()>
                <span class="rewind-row-here">{ "here" }</span>
            </Show>
        </button>
    }
    .into_any()
}

#[cfg(test)]
mod fan_angles_tests {
    use super::*;

    /// **D-fan-1/3 → D-snap-1/2 (v0.5.73, plan §15)**: the root fan is the
    /// **focus carousel**. For any `k` the focused rank sits at 0 (on top) and
    /// every other branch is in the lower half, `FAN_ARC_INSET` in from the
    /// horizontal at both ends; the `k = 3` case is exactly v0.5.72's
    /// 120°/240°. Nothing is ever edge-on, whatever the fan size.
    #[test]
    fn a_root_fan_hangs_below_the_focus() {
        for k in 1..=12usize {
            for focus in 0..k as i32 {
                let angles: Vec<f64> =
                    (0..k as i32).map(|r| fan_root_theta(r, focus, k)).collect();
                assert_eq!(angles[focus as usize], 0.0, "the focus is on top: k={k}");
                for (rank, a) in angles.iter().enumerate() {
                    if rank as i32 == focus {
                        continue;
                    }
                    // below the trunk: `cos a < 0` is the far half of the cone
                    // (`y = -q·cos a` puts it *below* the axis row)
                    assert!(
                        a.to_radians().cos() < 0.0,
                        "not in the lower half: k={k} rank={rank} a={a}"
                    );
                    assert!(
                        a.to_radians().cos().abs() >= FAN_EDGE_ON,
                        "edge-on: k={k} rank={rank} a={a}"
                    );
                    // and inside the inset arc (never past the horizontal)
                    let deg = a.rem_euclid(360.0);
                    assert!(
                        deg > FAN_ARC_INSET && deg < 360.0 - FAN_ARC_INSET,
                        "outside the arc: k={k} a={a}"
                    );
                }
                // the walk order: a positive wheel (a rising phase) visits the
                // ranks in fan order, wrapping
                let mut ahead: Vec<(f64, i32)> = (0..k as i32)
                    .map(|r| ((-wrap180(fan_root_theta(r, focus, k))).rem_euclid(360.0), r))
                    .collect();
                ahead.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                for w in 1..k as i32 {
                    let want = (focus + w) % k as i32;
                    assert_eq!(
                        ahead[w as usize].1, want,
                        "walk order: k={k} focus={focus} step={w}"
                    );
                }
            }
        }
        // the three-branch case is the v0.5.72 picture, unchanged
        assert_eq!(wrap180(fan_root_theta(1, 0, 3)), -120.0);
        assert_eq!(wrap180(fan_root_theta(2, 0, 3)), 120.0);
    }

    /// **D-fan-4**: a nested fan hangs *below* its parent — centred on 180°, so
    /// no child ever runs along the parent's own ray (which is what hid a
    /// +30° child behind its parent's next bead) — and it is off the camera's
    /// axis too.
    #[test]
    fn a_nested_fan_hangs_below_its_parent() {
        for k in 1..=12usize {
            let step = fan_step(k, true);
            assert!((step - (360.0 / k as f64).min(120.0)).abs() < 1e-9, "k={k}");
            for j in 0..k {
                let a = fan_nested_theta(j, k).rem_euclid(360.0);
                assert!(a > 5.0 && a < 355.0, "on the parent's ray: k={k} a={a}");
                assert!(
                    a.to_radians().cos().abs() >= FAN_EDGE_ON,
                    "edge-on: k={k} a={a}"
                );
            }
            assert!(
                (fan_nested_theta(0, 1) - 180.0).abs() < 1e-9,
                "a lone child sits straight below its parent"
            );
        }
    }
}
