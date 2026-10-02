//! v0.5.52 P1: the client-side event WINDOW (plan §9.13/§9.14).
//!
//! The server ships only the last page of events on connect and pages
//! backwards on demand, but the client never dropped anything: in a live
//! session `events` grew without bound. Every appended event then paid
//! the derived-state / keying cost of the whole history (the "freezes
//! after a while" bug), and the tab's heap grew with it.
//!
//! This module keeps the window bounded:
//!
//!   * trim once the window passes `EVENTS_TRIM_AT` (K + slack) down to
//!     `EVENTS_CAP` (K);
//!   * only when the dropped PREFIX does not intersect what the reader
//!     can see — dropping the oldest events is then invisible (at the
//!     bottom the browser re-clamps, mid-window we shift `scrollTop` by
//!     the measured height of the removed cards on the frame the DOM
//!     actually shrank);
//!   * always past `EVENTS_HARD_CAP`, so the window can never grow
//!     without bound even if the reader parks in old history forever;
//!   * bookkeeping stays exact: `hist_oldest_line` advances by the
//!     dropped line count (1 line = 1 event, which is what the server's
//!     paging counts), `earlier_loaded` shrinks, `hist_has_more` stays,
//!     and the round pin / ctx bookkeeping / derived signals are
//!     re-derived from the trimmed window.

use std::cell::Cell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::HtmlElement;

use crate::model::AppState;

/// Target window size (≈3 server pages; the server caps a page at 1000
/// events — `bin/rushi-web/src/sessions.rs`).
pub const EVENTS_CAP: usize = 3000;
/// Start trimming once the window passes this. Trimming in chunks keeps
/// the (one-off) re-key + re-derive cost off the per-event path.
pub const EVENTS_TRIM_AT: usize = EVENTS_CAP + 500;
/// Absolute ceiling: past this the window is trimmed even when the drop
/// range overlaps the viewport (a parked reader cannot make it grow
/// without bound). 1.5×K.
pub const EVENTS_HARD_CAP: usize = EVENTS_CAP * 3 / 2;
/// Height slack (px) within which the reader still counts as "at the
/// bottom": there the browser re-clamps `scrollTop` itself, so the trim
/// needs no compensation at all.
const BOTTOM_SLACK_PX: f64 = 8.0;
/// After a `load_earlier` prepend the pile engine is mid-compensation
/// for a few frames; do not trim during that window.
const PREPEND_QUIET_MS: f64 = 500.0;

thread_local! {
    /// Scroll compensation owed for trims whose DOM flush had not
    /// landed yet (px).
    static PENDING_COMP: Cell<f64> = const { Cell::new(0.0) };
    /// True while a re-check loop is scheduled.
    static COMP_RUNNING: Cell<bool> = const { Cell::new(false) };
    /// `load_earlier` prepend quiet period (ms since epoch).
    static QUIET_UNTIL: Cell<f64> = const { Cell::new(0.0) };
}

/// v0.5.52: called by the WS `history_page` handler right before it
/// merges an earlier page in.
pub fn note_prepend() {
    QUIET_UNTIL.set(now_ms() + PREPEND_QUIET_MS);
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

fn transcript() -> Option<HtmlElement> {
    let doc = web_sys::window()?.document()?;
    let el = doc.get_element_by_id("transcript")?;
    Some(el.unchecked_into::<HtmlElement>())
}

fn at_bottom(t: &HtmlElement) -> bool {
    (t.scroll_top() as f64 + t.client_height() as f64)
        >= (t.scroll_height() as f64 - BOTTOM_SLACK_PX)
}

/// Index of the first card the reader can actually see (rendered cards
/// only: `ext_status` events render an empty view, so the engine — and
/// this trim — count card k as the k-th NON-ext_status event).
fn first_visible_card(t: &HtmlElement) -> Option<usize> {
    let coll = t.children();
    let n = coll.length();
    let top = t.scroll_top() as f64;
    let bottom = top + t.client_height() as f64;
    let mut idx = 0usize;
    for i in 0..n {
        let Some(el) = coll.item(i) else { continue };
        let cl = el.class_list();
        if !cl.contains("event") || cl.contains("ev-streaming") {
            continue;
        }
        let card: HtmlElement = el.unchecked_into();
        let ct = card.offset_top() as f64;
        let ch = card.offset_height() as f64;
        // A card counts as visible when it overlaps the viewport band.
        if ct + ch > top && ct < bottom {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

/// Total height of the first `m` rendered cards (the ones about to be
/// dropped).
fn dropped_cards_height(t: &HtmlElement, m: usize) -> f64 {
    if m == 0 {
        return 0.0;
    }
    let coll = t.children();
    let n = coll.length();
    let mut idx = 0usize;
    let mut h = 0.0;
    for i in 0..n {
        let Some(el) = coll.item(i) else { continue };
        let cl = el.class_list();
        if !cl.contains("event") || cl.contains("ev-streaming") {
            continue;
        }
        if idx >= m {
            break;
        }
        let card: HtmlElement = el.unchecked_into();
        h += card.offset_height() as f64;
        idx += 1;
    }
    h
}

/// Shift `scrollTop` up by the height of the removed cards, once the DOM
/// has actually shrunk (the flush may land a frame later). Only used when
/// the reader is NOT at the bottom — there the browser's own clamp
/// already lands on the same value.
fn schedule_compensation(owed: f64, shrink_before: f64) {
    if owed <= 0.0 {
        return;
    }
    PENDING_COMP.set(PENDING_COMP.get() + owed);
    if COMP_RUNNING.get() {
        return;
    }
    COMP_RUNNING.set(true);
    tick_compensation(shrink_before, 0);
}

fn tick_compensation(shrink_before: f64, tries: u32) {
    let owed = PENDING_COMP.get();
    if owed <= 0.0 {
        COMP_RUNNING.set(false);
        return;
    }
    let Some(t) = transcript() else {
        PENDING_COMP.set(0.0);
        COMP_RUNNING.set(false);
        return;
    };
    let flushed = (t.scroll_height() as f64) < shrink_before - 0.5;
    if flushed || tries >= 8 {
        let top = (t.scroll_top() as f64 - owed).max(0.0);
        t.set_scroll_top(top as i32);
        PENDING_COMP.set(0.0);
        COMP_RUNNING.set(false);
        return;
    }
    if let Some(w) = web_sys::window() {
        let cb = Closure::once_into_js(move || tick_compensation(shrink_before, tries + 1));
        let _ = w.request_animation_frame(cb.unchecked_ref());
    } else {
        COMP_RUNNING.set(false);
    }
}

thread_local! {
    /// Diagnostics: number of trims that needed scroll compensation and
    /// the last amount owed (px).
    static COMPS: Cell<u64> = const { Cell::new(0) };
    static LAST_OWED: Cell<f64> = const { Cell::new(0.0) };
}

/// The one entry point: called after a live event was appended (and is
/// safe to call at any time — it returns immediately when the window is
/// under the threshold).
pub fn maybe_trim(state: &AppState) {
    // v0.5.52: untracked — this runs once per live event OUTSIDE any
    // reactive scope; a tracked read here made Leptos log a warning for
    // every single event (measured: 1.0 warning/event, 200/200 in a
    // 200-event batch), which floods the console and costs CPU in a
    // long-running tab.
    let len = state.events.with_untracked(|v| v.len());
    if len <= EVENTS_TRIM_AT {
        return;
    }
    let forced = len > EVENTS_HARD_CAP;
    if !forced {
        if state.loading_earlier.get_untracked() {
            return;
        }
        if now_ms() < QUIET_UNTIL.get() {
            return;
        }
    }
    let drop_n = len - EVENTS_CAP;
    // What the dropped prefix contains: rendered events (⇒ DOM cards)
    // and round boundaries (⇒ the pinned round index can shift).
    let (rendered, rounds_dropped) = state.events.with_untracked(|v| {
        let mut rendered = 0usize;
        let mut rounds = 0usize;
        for e in v.iter().take(drop_n) {
            let t = e.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if t == "ext_status" {
                continue;
            }
            rendered += 1;
            if t == "user_message" {
                rounds += 1;
            }
        }
        (rendered, rounds)
    });

    // Is the drop range disjoint from what the reader sees?
    let mut removed_h = 0.0;
    let mut need_comp = false;
    let view_ok = match transcript() {
        Some(t) => {
            let first = first_visible_card(&t).unwrap_or(usize::MAX);
            let ok = first >= rendered;
            if ok {
                let bottom = at_bottom(&t);
                if !bottom {
                    removed_h = dropped_cards_height(&t, rendered);
                    need_comp = removed_h > 0.0;
                }
            }
            ok
        }
        None => true,
    };
    if !view_ok && !forced {
        return;
    }
    let shrink_before = transcript().map(|t| t.scroll_height() as f64).unwrap_or(0.0);

    // ── mutate the window ───────────────────────────────────────────
    state.events.update(|v| {
        v.drain(0..drop_n);
    });
    DROPS.with(|d| d.set(d.get() + 1));
    DROPPED_TOTAL.with(|d| d.set(d.get() + drop_n as u64));
    // v0.5.33 keying is positional: a prefix write invalidates every card,
    // so the `For` rebuilds against the trimmed list (the same reason the
    // history frame / prepend bump the generation).
    state.ev_gen.update(|g| *g += 1);
    if state.hist_oldest_line.get_untracked() > 0 {
        state.hist_oldest_line.update(|l| *l += drop_n as u64);
    }
    // v0.5.52: the dropped prefix is NOT gone — it is still on disk above
    // the window, so "load earlier" must stay reachable even when the log
    // used to fit entirely ("has_more" started false). Without this the
    // trim would silently make older events unreachable.
    if state.hist_oldest_line.get_untracked() > 1 {
        state.hist_has_more.set(true);
    }
    state
        .earlier_loaded
        .update(|n| *n = n.saturating_sub(drop_n as u64));
    // The pinned round moves up with the window (or is dropped entirely).
    if let Some(i) = state.view_round.get_untracked() {
        if rounds_dropped > 0 || drop_n > 0 {
            if i < rounds_dropped {
                state.view_round.set(None);
            } else if rounds_dropped > 0 {
                state.view_round.set(Some(i - rounds_dropped));
            }
        }
    }
    // Re-derive everything that is a function of the window.
    let (ctx, ctxk) = state.events.with(|v| crate::ws::rebuild_ctx_bookkeeping(v));
    state.ctx_used.set(ctx);
    state.rounds_ctxk.set(ctxk);
    let pending = state.events.with(|v| crate::ws::rebuild_tool_pending(v));
    state.tool_pending.set(pending);
    state.rebuild_derived();
    if need_comp {
        COMPS.with(|c| c.set(c.get() + 1));
        LAST_OWED.with(|c| c.set(removed_h));
        schedule_compensation(removed_h, shrink_before);
    }
    crate::pile::on_change();
}

// ── diagnostics (probe suites) ──────────────────────────────────────
//
// `window.__rushiWin()` exposes the window bounds to the CDP probes so
// the trimming bookkeeping can be asserted from outside: the event
// count must stay under the ceiling, and `oldest` must equal the file
// line of the window's FIRST event (that is what makes "load earlier"
// exact — no duplicate, no gap).

thread_local! {
    static DBG_FN: std::cell::RefCell<Option<Closure<dyn Fn() -> String>>> =
        const { std::cell::RefCell::new(None) };
}

/// v0.5.52: install `window.__rushiWin()` (called once at mount).
pub fn register_debug_hook(state: AppState) {
    let Some(w) = web_sys::window() else { return };
    if DBG_FN.with(|c| c.borrow().is_some()) {
        return;
    }
    let f = Closure::new(move || -> String {
        let (len, first_type, first_brief) = state.events.with(|v| {
            let e0 = v.first();
            (
                v.len(),
                e0.and_then(|e| e.get("type")).and_then(|t| t.as_str()).unwrap_or("").to_string(),
                e0.and_then(|e| e.get("content"))
                    .and_then(|c| c.as_str())
                    .or_else(|| e0.and_then(|e| e.get("value")).and_then(|c| c.as_str()))
                    .or_else(|| e0.and_then(|e| e.get("id")).and_then(|c| c.as_str()))
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>(),
            )
        });
        format!(
            "cap={} hard={} len={} oldest={} has_more={} earlier_loaded={} drops={} dropped_total={} comps={} owed={:.1} first_type={} first_brief={}",
            EVENTS_CAP,
            EVENTS_HARD_CAP,
            len,
            state.hist_oldest_line.get_untracked(),
            state.hist_has_more.get_untracked(),
            state.earlier_loaded.get_untracked(),
            DROPS.with(|d| d.get()),
            DROPPED_TOTAL.with(|d| d.get()),
            COMPS.with(|c| c.get()),
            LAST_OWED.with(|c| c.get()),
            first_type,
            // keep it one line: the probes split on ' | ' for fields that
            // may contain spaces
            first_brief.replace('\n', " / "),
        )
    });
    let _ = js_sys::Reflect::set(
        w.as_ref(),
        &wasm_bindgen::JsValue::from_str("__rushiWin"),
        f.as_ref().unchecked_ref(),
    );
    DBG_FN.with(|c| *c.borrow_mut() = Some(f));
}

thread_local! {
    /// Number of trims performed (diagnostics).
    static DROPS: Cell<u64> = const { Cell::new(0) };
    /// Number of events dropped in total (diagnostics).
    static DROPPED_TOTAL: Cell<u64> = const { Cell::new(0) };
}
