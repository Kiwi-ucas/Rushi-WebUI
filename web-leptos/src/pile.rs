//! Phase 4: the scroll-coupled card pile engine + keyboard lift
//! (port of the legacy `applyView` / `syncCardUnfold` / `syncCardShrink` /
//! `setPileOpen` / `syncInputGutter`, the document/session-menu listeners,
//! the `/` focus shortcut, and the visualViewport keyboard lift).
//!
//! The Leptos `For` keeps every event card in the DOM; this module owns
//! the imperative layer over the `#transcript .event` nodes: folded
//! cards are 52px compact rows (the scroll range), the pile itself is
//! the newest folded row glued to the transcript top by a per-frame
//! transform. Folding is scroll-direction gated and position based, so
//! the pile and the list move as one coupled motion.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::{
    DomRectReadOnly, EventTarget, HtmlElement, KeyboardEvent, Selection,
};
use js_sys::Reflect;

use leptos::prelude::{GetUntracked, Set};

use crate::model::{compute_rounds, AppState};

/// Pile anchor: a card whose slot top rises to this screen y folds
/// (the transcript's top padding).
const PILE_TOP: f64 = 16.0;
/// Hysteresis: a folded card only deals back out once its slot has
/// dropped this far below the anchor (no flicker at the boundary).
const PILE_BAND: f64 = 12.0;
/// Layout footprint of a compact row: 64px face, 0 bottom margin
/// (flush rows — the row pitch stays 64px).
const COMPACT_ROW_H: f64 = 64.0;
/// Max deck layers pinned at the transcript top: the readable deck-top
/// card (newest, lowest of the cascade) plus this many older folded
/// rows peeking ABOVE it as 5px top edges (inverted stack: old on top).
const DECK_SHOW: usize = 4;
/// How far each deck layer peeks ABOVE the card in front of it
/// (inverted: the oldest visible layer sits 3*5=15px above the
/// newest, i.e. PILE_TOP-15 — still within the transcript's top
/// padding, nothing clips).
const DECK_PEEK: f64 = 5.0;
/// Margin-bottom of a full (queue) row: the base .event margin.
/// Compact rows are flush (0 margin; pitch = COMPACT_ROW_H). The
/// queue-still drift when a card folds: nat + FULL_ROW_MARGIN - COMPACT_ROW_H.
const FULL_ROW_MARGIN: f64 = 12.0;

// ── engine state ──────────────────────────────────────────────────
struct PileState {
    state: AppState,
    transcript: Option<HtmlElement>,
    scroll_spacer: Option<HtmlElement>,
    input_module: Option<HtmlElement>,
    /// The pile's top card (glued by transform).
    pile_face: Option<HtmlElement>,
    /// Cards currently pinned into the top deck (for clearing stale
    /// pinning when the deck composition changes).
    deck_layers: Vec<(HtmlElement, i32)>,
    /// Click-expanded override: deals every row regardless of scroll.
    pile_open: bool,
    /// Last observed scroll top (deal/dock are scroll-direction gated).
    last_stop: f64,
    /// Card currently height-shrunk at the cut line.
    shrunken_card: Option<HtmlElement>,
    /// Summary card of the current view (never folds; the pile stops
    /// there).
    current_summary: Option<HtmlElement>,
    last_applied_summary: Option<HtmlElement>,
    last_events_len: usize,
    last_view: Option<usize>,
    /// visualViewport keyboard-lift state.
    kb_open: bool,
    /// One rAF step is already scheduled this frame.
    step_scheduled: bool,
    /// Full engine steps run since init (diagnostics).
    steps: u64,
    /// One-shot console trace fired.
    traced: bool,
    /// Last active session (session-change scroll, diagnostics).
    last_active: Option<String>,
    /// The initial fold actually ran at least once (self-heal: while
    /// false, every step forces a re-fold so a DOM-not-ready first
    /// frame can't leave the cards permanently unfolded).
    fold_applied: bool,
    /// Park the transcript at the last message on the next DOM-ready
    /// step (set by `on_history_loaded`, e.g. right after a session's
    /// history lands).
    park_bottom: bool,
    /// One-shot stall alarm already fired (DOM-lag gate retried 90
    /// frames without the For DOM catching up).
    stall_reported: bool,
    /// Ring of the last few deal decisions (diagnostics).
    deal_dbg_hist: Vec<String>,
    /// Accumulated queue-still scroll correction from async commit_fold
    /// (pending-fold) that will be applied at the next step's gluing pass.
    pending_corr: f64,
    // Closures kept alive for the app lifetime (wasm GC):
    raf: Option<Closure<dyn Fn()>>,
    scroll_cb: Option<Closure<dyn Fn()>>,
    click_cb: Option<Closure<dyn Fn(web_sys::Event)>>,
    resize_cb: Option<Closure<dyn Fn()>>,
    transend_cb: Option<Closure<dyn Fn()>>,
    doc_click_cb: Option<Closure<dyn Fn(web_sys::Event)>>,
    doc_key_cb: Option<Closure<dyn Fn(KeyboardEvent)>>,
    vv: Option<Closure<dyn Fn()>>,
    kb_interval: Option<gloo_timers::callback::Interval>,
}

thread_local! {
    static PILE: RefCell<Option<Rc<RefCell<PileState>>>> = const { RefCell::new(None) };
    /// Timers that must outlive the frame that created them.
    static LEAKED: RefCell<Vec<Box<gloo_timers::callback::Timeout>>> = const { RefCell::new(Vec::new()) };
    /// The `window.__rushiPile` console diagnostic (kept alive).
    static DBG_FN: RefCell<Option<Closure<dyn Fn() -> JsValue>>> = const { RefCell::new(None) };
    /// Last panic message (set by the app panic hook in lib.rs; a
    /// wasm panic in an rAF callback silently kills the engine loop,
    /// so the report exposes it).
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
    /// init ran before `#transcript` existed in the DOM; pending
    /// retry `(AppState, generation)`. A timer re-runs init every
    /// 150 ms (up to 20 times); the generation guard drops stale
    /// timers. The Transcript effect also re-calls init on signal
    /// changes, but that may never come on its own.
    static PENDING_INIT: RefCell<Option<(AppState, u32)>> = const { RefCell::new(None) };
}

/// Called from the app panic hook (lib.rs): records the last panic
/// message so `__rushiPile()` can surface it.
pub fn record_panic(msg: &str) {
    LAST_PANIC.with(|c| *c.borrow_mut() = Some(msg.to_string()));
}

fn with_pile<T>(f: impl FnOnce(&RefCell<Option<Rc<RefCell<PileState>>>>) -> T) -> T {
    PILE.with(f)
}

// ── entry points ───────────────────────────────────────────────────
/// Called once from the Transcript mount path; registers every DOM
/// listener the engine (and the ported document-level handlers) need.
pub fn init(state: AppState) {
    with_pile(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_some() {
            return;
        }
        let Some(w) = web_sys::window() else { return };
        let Some(doc) = w.document() else { return };
        let transcript = doc
            .get_element_by_id("transcript")
            .map(|e| e.unchecked_into::<HtmlElement>());
        let scroll_spacer = doc
            .get_element_by_id("scroll-spacer")
            .map(|e| e.unchecked_into::<HtmlElement>());
        let input_module = doc
            .get_element_by_id("input-module")
            .map(|e| e.unchecked_into::<HtmlElement>());
        if transcript.is_none() {
            // DOM not ready yet (mount not flushed on this tick).
            // Stash the state and retry on a 150 ms timer (up to
            // ~3 s); the Transcript effect re-calls init on signal
            // changes too, but that may never come on its own — the
            // generation guard drops stale timers.
            let n = PENDING_INIT
                .with(|p| p.borrow().as_ref().map(|(_, k)| *k).unwrap_or(0))
                + 1;
            PENDING_INIT.with(|p| *p.borrow_mut() = Some((state, n)));
            if n <= 20 {
                let to = gloo_timers::callback::Timeout::new(150, move || {
                    PENDING_INIT.with(|p| {
                        if let Some((s, k)) = p.borrow().clone() {
                            if k == n {
                                init(s);
                            }
                        }
                    });
                });
                LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
            }
            return;
        }

        let mut ps = PileState {
            state,
            transcript,
            scroll_spacer,
            input_module,
            pile_face: None,
            deck_layers: Vec::new(),
            pile_open: false,
            last_stop: 0.0,
            shrunken_card: None,
            current_summary: None,
            last_applied_summary: None,
            last_events_len: 0,
            last_view: None,
            kb_open: false,
            step_scheduled: false,
            steps: 0,
            traced: false,
            last_active: state.active_session.get_untracked(),
            fold_applied: false,
            park_bottom: false,
            stall_reported: false,
            deal_dbg_hist: Vec::new(),
            pending_corr: 0.0,
            raf: None,
            scroll_cb: None,
            click_cb: None,
            resize_cb: None,
            transend_cb: None,
            doc_click_cb: None,
            doc_key_cb: None,
            vv: None,
            kb_interval: None,
        };
        if let Some(t) = &ps.transcript {
            ps.last_stop = t.scroll_top() as f64;
        }

        let raf = Closure::<dyn Fn()>::new(|| on_frame());
        ps.raf = Some(raf);

        // scroll → rAF-throttled unfold+shrink (legacy handler).
        if let Some(t) = &ps.transcript {
            let vt: EventTarget = t.clone().unchecked_into();
            let on_scroll = Closure::<dyn Fn()>::new(|| schedule_step());
            let _ = vt.add_event_listener_with_callback("scroll", on_scroll.as_js_value().unchecked_ref::<js_sys::Function>());
            ps.scroll_cb = Some(on_scroll);
        }

        // transcript click → pile open/close (legacy handler).
        if let Some(t) = &ps.transcript {
            let vt: EventTarget = t.clone().unchecked_into();
            let on_click = Closure::<dyn Fn(web_sys::Event)>::new(|e| on_transcript_click(&e));
            let _ = vt.add_event_listener_with_callback("click", on_click.as_js_value().unchecked_ref::<js_sys::Function>());
            ps.click_cb = Some(on_click);
        }

        // window resize + sidebar transitionend → input gutter (legacy).
        {
            let vt: EventTarget = w.clone().unchecked_into();
            let on_resize = Closure::<dyn Fn()>::new(|| sync_input_gutter());
            let _ = vt.add_event_listener_with_callback("resize", on_resize.as_js_value().unchecked_ref::<js_sys::Function>());
            ps.resize_cb = Some(on_resize);
        }
        if let Some(sb) = doc.get_element_by_id("sidebar").map(|e| e.unchecked_into::<HtmlElement>()) {
            let vt: EventTarget = sb.clone().unchecked_into();
            let on_te = Closure::<dyn Fn()>::new(|| sync_input_gutter());
            let _ = vt.add_event_listener_with_callback("transitionend", on_te.as_js_value().unchecked_ref::<js_sys::Function>());
            ps.transend_cb = Some(on_te);
            let _ = sb;
        }

        // document click (close session menu outside) + keydown
        // (Escape closes the menu; `/` focuses the input) — legacy
        // document-level listeners.
        {
            let vt: EventTarget = doc.clone().unchecked_into();
            let ms = state.menu_session;
            let on_doc_click = Closure::<dyn Fn(web_sys::Event)>::new(move |e: web_sys::Event| {
                if ms.get_untracked().is_none() {
                    return;
                }
                let Some(doc2) = web_sys::window().and_then(|w| w.document()) else { return };
                let Some(menu) = doc2.get_element_by_id("sess-menu") else {
                    ms.set(None);
                    return;
                };
                let Some(target) = e.target() else {
                    ms.set(None);
                    return;
                };
                let t_node: web_sys::Node = target.unchecked_into();
                let m_node: web_sys::Node = menu.unchecked_into();
                if !m_node.contains(Some(&t_node)) {
                    ms.set(None);
                }
            });
            let _ = vt.add_event_listener_with_callback("click", on_doc_click.as_js_value().unchecked_ref::<js_sys::Function>());
            ps.doc_click_cb = Some(on_doc_click);

            let on_key = Closure::<dyn Fn(KeyboardEvent)>::new(move |e: KeyboardEvent| {
                if e.key() == "Escape" {
                    ms.set(None);
                    return;
                }
                if e.key() == "/" {
                    let Some(doc2) = web_sys::window().and_then(|w| w.document()) else { return };
                    let Some(input) = doc2.get_element_by_id("msg-input") else { return };
                    let is_input = doc2.active_element().is_some_and(|a| a == input);
                    if !is_input {
                        e.prevent_default();
                        let _ = input.unchecked_into::<HtmlElement>().focus();
                    }
                }
            });
            let _ = vt.add_event_listener_with_callback("keydown", on_key.as_js_value().unchecked_ref::<js_sys::Function>());
            ps.doc_key_cb = Some(on_key);
        }

        // visualViewport keyboard lift (legacy onVV listener + interval).
        if let Some(vv) = w.visual_viewport() {
            let coarse = w
                .match_media("(hover: none), (pointer: coarse)")
                .ok()
                .flatten()
                .map(|m| m.matches())
                .unwrap_or(false);
            if coarse {
                if let Some(body) = doc.body() {
                    let _ = body.class_list().add_1("kb-anim");
                }
            }
            let on_vv = Closure::<dyn Fn()>::new(|| on_vv_event());
            let _ = vv.set_onresize(Some(on_vv.as_js_value().unchecked_ref::<js_sys::Function>()));
            let _ = vv.set_onscroll(Some(on_vv.as_js_value().unchecked_ref::<js_sys::Function>()));
            ps.vv = Some(on_vv);
            ps.kb_interval = Some(gloo_timers::callback::Interval::new(500, || on_vv_event()));
            // NOTE: do NOT call on_vv_event() here — it borrows PILE,
            // which init currently holds mutably; a re-entrant borrow
            // panics, and a wasm trap while borrowed poisons the cell
            // permanently (Drop never runs). It's called after the
            // with_pile block below.
        }

        cell.replace(Rc::new(RefCell::new(ps)));
        register_debug_hook(&w);
    });
    sync_input_gutter();
    on_vv_event();
    on_change();
}

/// Last recorded panic message, or "none" (diagnostics).
fn last_panic_str() -> String {
    LAST_PANIC.with(|c| c.borrow().clone().unwrap_or_else(|| "none".to_string()))
}

/// Console diagnostics: `window.__rushiPile()` returns a one-line
/// engine-state snapshot (useful when the pile behaviour looks wrong
/// and the app otherwise works).
fn register_debug_hook(w: &web_sys::Window) {
    if DBG_FN.with(|c| c.borrow().is_some()) {
        return;
    }
    let fn_ = Closure::new(move || -> JsValue {
        let report = with_pile(|cell| {
            let st = cell.borrow();
            let st = st.as_ref();
            let st = st.as_ref().map(|rc| rc.borrow());
            match st {
                Some(st) => {
                    let cards = iter_cards(&st);
                    let compact = cards
                        .iter()
                        .filter(|el| el.class_list().contains("compact"))
                        .count();
                    format!(
                        "init=1 steps={} fold_applied={} compact={}/{} events={} cards={} pile_face={} pile_open={} kb_open={} summary={} park_bottom={} stall_reported={} active={:?} view={:?} last_panic={} dbg=[{}]",
                        st.steps,
                        st.fold_applied,
                        compact,
                        cards.len(),
                        st.state.events.get_untracked().len(),
                        cards.len(),
                        st.pile_face.is_some(),
                        st.pile_open,
                        st.kb_open,
                        st.current_summary.is_some(),
                        st.park_bottom,
                        st.stall_reported,
                        st.state.active_session.get_untracked().as_deref(),
                        st.last_view,
                        last_panic_str(),
                        st.deal_dbg_hist.join(" | "),
                    )
                }
                None => format!(
                    "init=0 last_panic={}",
                    last_panic_str()
                ),
            }
        });
        JsValue::from_str(&report)
    });
    let v = fn_.as_ref().clone();
    let _ = Reflect::set(w, &JsValue::from_str("__rushiPile"), &v);
    DBG_FN.with(|c| *c.borrow_mut() = Some(fn_));
}

/// Called reactively (Transcript's effect): on any event-stream /
/// view / session change, run a coalesced rAF step.
pub fn on_change() {
    with_pile(|cell| {
        let st = cell.borrow();
        if st.is_some() {
            schedule_step();
        }
    });
}

/// Call right after a session's history lands (ws.rs): schedule a
/// step and park the transcript at the last message — selecting a
/// session must land on the newest message, not the top.
pub fn on_history_loaded() {
    with_pile(|cell| {
        let st = cell.borrow();
        let Some(st) = st.as_ref() else { return };
        let mut st = st.borrow_mut();
        st.park_bottom = true;
    });
    on_change();
}

/// Run the full engine step on the next animation frame (coalesced).
/// `st` must be the caller's `&mut PileState` (inner lock already
/// held) — this function must NOT re-borrow the thread-local cell.
fn schedule_step_locked(st: &mut PileState) {
    if st.step_scheduled {
        return;
    }
    st.step_scheduled = true;
    if let (Some(w), Some(raf)) = (web_sys::window(), st.raf.as_ref()) {
        let _ = w.request_animation_frame(raf.as_js_value().unchecked_ref::<js_sys::Function>());
    }
}

/// Coalesced rAF scheduling from outside the engine lock.
fn schedule_step() {
    with_pile(|cell| {
        let st = cell.borrow();
        let Some(st) = st.as_ref() else { return };
        let mut st = st.borrow_mut();
        schedule_step_locked(&mut st);
    });
}

fn on_frame() {
    with_pile(|cell| {
        let st = cell.borrow();
        let Some(st) = st.as_ref() else { return };
        let mut st = st.borrow_mut();
        st.step_scheduled = false;
        step_full(&mut st);
    });
}

// ── full step: measure, tags, view, unfold, shrink, auto-scroll ──
fn step_full(st: &mut PileState) {
    let events = st.state.events.get_untracked();
    let view = st.state.view_round.get_untracked();
    let active = st.state.active_session.get_untracked();
    let active_some = active.is_some();
    st.steps += 1;

    // Session switch (or delete) resets the pile bookkeeping.
    if events.is_empty() {
        st.pile_face = None;
        set_pile_open_flag(st, false);
        st.shrunken_card = None;
        st.current_summary = None;
        st.last_applied_summary = None;
        st.last_events_len = 0;
        st.last_view = view;
        st.last_active = active.clone();
        st.fold_applied = false;
        release_shrink(st);
        set_spacer_height(st, 0.0);
        return;
    }

    // 1. DOM card inventory: transcript children are
    //    [card, card, ..., #scroll-spacer]; ext_status events render
    //    an empty view (no DOM node), so card k ↔ the k-th
    //    non-ext_status event.
    let card_events: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.get("type").and_then(|t| t.as_str()) != Some("ext_status"))
        .map(|(i, _)| i)
        .collect();

    let cards = iter_cards(st);
    if cards.len() < card_events.len() {
        // Leptos hasn't flushed the new card nodes yet. Run the scroll
        // bookkeeping NOW so parking is not blocked, then retry the
        // fold on the next frame.
        step_scrolls(st, &events, &active, view);
        // Stall watchdog: ~1.5 s of retries with the DOM still not
        // catching up = something is broken (engine death, Leptos
        // DOM mismatch). Make it loud, one-shot.
        if !st.stall_reported && st.steps >= 90 {
            st.stall_reported = true;
            let _ = js_sys::eval(&format!(
                "console.error('[rushi] pile stalled: DOM cards {} < expected {} after {} frames, last_panic={} — run __rushiPile()')",
                cards.len(),
                card_events.len(),
                st.steps,
                last_panic_str(),
            ));
        }
        schedule_step_locked(st);
        return;
    }

    // One-shot trace so a silent engine death is visible in the
    // console (the rAF loop is the only thing keeping this running).
    if !st.traced && !cards.is_empty() {
        st.traced = true;
        let _ = js_sys::eval(&format!(
            "console.log('[rushi] pile ok: cards={}, events={}, view={:?}, active={:?}')",
            cards.len(),
            events.len(),
            view,
            active,
        ));
    }

    // 2. isSum tags + natural-height measurement for new cards
    //    (legacy addEvent/markSummary, re-derivable from the stream).
    //    A round's summary is its last *rendered* card, so skip
    //    ext_status (they have no DOM node).
    let rounds = compute_rounds(&events);
    // Legacy applyView clamp: an out-of-range round view falls back to
    // live (e.g. a stale view_round after a session switch).
    let view = view.filter(|&i| i < rounds.len());
    let is_sum: std::collections::HashSet<usize> = rounds
        .iter()
        .filter_map(|r| {
            (r.start..r.end)
                .rev()
                .find(|i| events[*i].get("type").and_then(|t| t.as_str()) != Some("ext_status"))
        })
        .collect();
    for (k, el) in cards.iter().enumerate() {
        let ev_idx = *card_events.get(k).unwrap_or(&usize::MAX);
        if is_sum.contains(&ev_idx) {
            let _ = el.set_attribute("data-isSum", "1");
        } else {
            let _ = el.remove_attribute("data-isSum");
        }
        if !el.has_attribute("data-natH") {
            // Freshly mounted at full height: measure before the view
            // logic folds it (legacy: dataset.natH at addEvent time).
            let h = el.offset_height() as f64;
            let _ = el.set_attribute("data-natH", &h.to_string());
        }
    }

    // 3. apply view (legacy applyView): pick the summary card,
    //    re-fold when it changed, hide cards after it. The summary is
    //    the last *rendered* card of the view (ext_status skipped).
    let summary_ev: Option<usize> = if !active_some {
        None
    } else {
        match view {
            None => card_events.last().copied(),
            Some(i) => rounds
                .get(i)
                .and_then(|r| (r.start..r.end).rev().find(|e| card_events.binary_search(e).is_ok())),
        }
    };
    let summary_el = summary_ev.and_then(|ei| {
        card_events
            .iter()
            .position(|&c| c == ei)
            .and_then(|k| cards.get(k))
            .cloned()
    });
    // Re-fold only when the visible card set actually changes: a
    // round-view switch, a session switch, or the very first fold
    // (self-heal: a DOM-not-ready first frame must not leave the
    // cards permanently unfolded). Live-view appends shift the
    // summary forward — they must NOT trigger a mass re-fold.
    let view_changed = st.last_view != view;
    let session_changed = st.last_active.as_ref() != active.as_ref();
    let mut reset_fold = view_changed || session_changed;
    if !st.fold_applied && summary_el.is_some() {
        reset_fold = true;
    }
    if reset_fold {
        set_pile_open_flag(st, false);
    }
    st.last_applied_summary = summary_el.clone();
    st.current_summary = summary_el.clone();

    for (k, el) in cards.iter().enumerate() {
        let ev_idx = *card_events.get(k).unwrap_or(&usize::MAX);
        let el_is_summary = summary_ev == Some(ev_idx);
        let after_summary = summary_ev.map(|s| ev_idx > s).unwrap_or(false);
        let cls = el.class_list();
        if el_is_summary {
            let _ = cls.remove_1("hid");
            let _ = cls.remove_1("compact");
            let _ = cls.remove_1("folding");
            let _ = cls.remove_1("fold-anim");
            let _ = cls.remove_1("deck-layer");
            let _ = cls.remove_1("deck-top");
            let _ = cls.remove_1("unfold-anim");
            let _ = el.style().remove_property("transform");
            let _ = el.style().remove_property("z-index");
            clear_inline(el);
            continue;
        }
        if after_summary {
            let _ = cls.add_1("hid");
            let _ = cls.remove_1("compact");
            let _ = cls.remove_1("folding");
            let _ = cls.remove_1("fold-anim");
            let _ = cls.remove_1("deck-layer");
            let _ = cls.remove_1("deck-top");
            let _ = el.style().remove_property("transform");
            let _ = el.style().remove_property("z-index");
            continue;
        }
        let _ = cls.remove_1("hid");
        if el.has_attribute("data-isSum") {
            // Exempt round summary: full card in the flow, never folds.
            let _ = cls.remove_1("compact");
            let _ = cls.remove_1("folding");
            let _ = cls.remove_1("fold-anim");
            let _ = cls.remove_1("deck-layer");
            let _ = cls.remove_1("deck-top");
            let _ = cls.remove_1("unfold-anim");
            let _ = el.style().remove_property("transform");
            let _ = el.style().remove_property("z-index");
            clear_inline(el);
            continue;
        }
        if reset_fold {
            add_compact(el);
        }
    }
    if reset_fold && summary_ev.is_some() {
        st.fold_applied = true;
    }

    // 4. scroll bookkeeping.
    step_scrolls(st, &events, &active, view);

    // 5. coupled syncs: the scroll-coupled fold/deal + the deck gluing,
    //    then the cut-line shrink keeps the bottom card's relief intact.
    sync_unfold(st);
    sync_shrink(st);
}

/// Scroll bookkeeping extracted so the DOM-lag gate path can also run
/// parking/auto-scroll without waiting for all card nodes to flush.
///
/// - Session change → scroll to bottom
/// - `grew` (new events in same session) → auto-scroll to bottom
/// - `park_bottom` flag → scroll to bottom + 150 ms re-park
/// - Round-view change → scroll to bottom
fn step_scrolls(
    st: &mut PileState,
    events: &Vec<serde_json::Value>,
    active: &Option<String>,
    view: Option<usize>,
) {
    // Session change with a loaded stream: park at the last message.
    if active.clone() != st.last_active {
        st.last_active = active.clone();
        park_to_bottom(st, true);
    }

    // Auto-scroll on new events while live, but ONLY if the user is
    // already at (or near) the bottom.  Yanking a user who is reading
    // mid-transcript — e.g. sitting at the summary card — to the very
    // bottom on every new event is the "jump past the summary" bug.
    let grew = events.len() > st.last_events_len;
    st.last_events_len = events.len();
    if grew && view.is_none() && active.is_some() {
        let near_bottom = st.transcript.as_ref().is_some_and(|t| {
            let dist = t.scroll_height() as f64 - t.scroll_top() as f64 - t.client_height() as f64;
            dist <= 80.0
        });
        if near_bottom {
            park_to_bottom(st, false);
        }
    }

    // Park at the last message (session select → history just landed,
    // or a live event arrived while the user sat at the bottom).
    if st.park_bottom {
        st.park_bottom = false;
        park_to_bottom(st, true);
    }

    // Round-view change: the summary is now the last visible card;
    // park it at the bottom (legacy setView smooth scroll — instant
    // here, same resting position).
    if st.last_view != view {
        st.last_view = view;
        if view.is_some() {
            park_to_bottom(st, false);
        }
    }
}

/// Scroll the transcript to its bottom edge. Uses `scroll-behavior:auto`
/// so programmatic scrolls land instantly (the CSS `scroll-behavior:smooth`
/// would animate and race with the 150 ms re-park).
///
/// `repark` schedules a second scroll 150 ms later to account for
/// Leptos still flushing new card nodes into the DOM.
fn park_to_bottom(st: &mut PileState, repark: bool) {
    let Some(t) = &st.transcript else { return };
    let _ = t.style().set_property("scroll-behavior", "auto");
    t.set_scroll_top(t.scroll_height());
    // Record the programmatic position so the next step sees a zero
    // delta (no stale corr flag to poison the user's next scroll).
    st.last_stop = t.scroll_top() as f64;
    if repark {
        let t2 = t.clone();
        let to = gloo_timers::callback::Timeout::new(150, move || {
            let _ = t2.style().set_property("scroll-behavior", "auto");
            let _ = t2.set_scroll_top(t2.scroll_height());
            // Record the position directly instead of corr_pending: a
            // no-op re-park leaves no stale flag that would zero the
            // user's next scroll delta.
            with_pile(|cell| {
                let s = cell.borrow();
                if let Some(s) = s.as_ref() {
                    s.borrow_mut().last_stop = t2.scroll_top() as f64;
                }
            });
        });
        LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
    }
}

// ── DOM traversal helpers ──────────────────────────────────────────
/// Live `#transcript .event` nodes (cards only; the spacer div and
/// any non-card children are skipped). Takes the caller's `&PileState`
/// so it never re-borrows the thread-local cell.
fn iter_cards(st: &PileState) -> Vec<HtmlElement> {
    let Some(t) = &st.transcript else { return Vec::new() };
    let coll = t.children();
    let n = coll.length();
    let mut out = Vec::new();
    for i in 0..n {
        if let Some(el) = coll.item(i) {
            if el.class_list().contains("event") {
                out.push(el.unchecked_into::<HtmlElement>());
            }
        }
    }
    out
}

fn clear_inline(el: &HtmlElement) {
    let s = el.style();
    let _ = s.set_property("height", "");
    let _ = s.set_property("overflow", "");
    let _ = s.set_property("transform", "");
    let _ = s.set_property("animation-delay", "");
    // A shrunk bottom card is hidden via inline `visibility`; a dealt
    // card must come back visible even if the shrink pass releases it
    // later.
    let _ = s.set_property("visibility", "");
    let _ = s.remove_property("--deal-dy");
    let _ = s.remove_property("--fold-dy");
    let _ = s.remove_property("z-index");
}

fn add_compact(el: &HtmlElement) {
    let cls = el.class_list();
    let _ = cls.add_1("compact");
    let _ = cls.remove_1("unfold-anim");
    let _ = cls.remove_1("fold-anim");
    let _ = cls.remove_1("folding");
    clear_inline(el);
    // force reflow before re-adding the animation class (legacy
    // `void el.offsetWidth`).
    let _ = el.offset_width();
}

fn attr_nat_h(el: &HtmlElement, default: f64) -> f64 {
    el.get_attribute("data-natH")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(default)
}

/// Commit a pending fold: the clip tuck has finished, so the card
/// swaps to the compact layout and its queue-still correction is
/// queued on the state — the next step's gluing pass applies the
/// scroll shift, so the layout shrink + correction + re-pin all land
/// in one frame (no snap). No-op when the fold was cancelled
/// (user scrolled out of the pile zone, view reset, session switch).
fn commit_fold(st: &mut PileState, el: &HtmlElement) {
    let cls = el.class_list();
    if !cls.contains("folding") {
        return; // cancelled — the card just stays in the queue
    }
    // The element was detached (session switch / view re-render):
    // nothing left to commit.
    let node: web_sys::Node = el.clone().unchecked_into();
    if !node.is_connected() {
        return;
    }
    let nat = attr_nat_h(el, 52.0);
    add_compact(el); // compact + removes folding/fold-anim + clears inline
    // Keep the card above the older deck layers until the gluing
    // assigns its own z-index on the next frame.
    let _ = el.style().set_property("z-index", "50");
    // Queue-still correction: compacting shrank the content above the
    // queue by (nat + FULL_ROW_MARGIN - COMPACT_ROW_H); the next step
    // applies the matching scroll shift and syncs `last_stop` (see the
    // pending_corr carry).
    st.pending_corr -= nat + FULL_ROW_MARGIN - COMPACT_ROW_H;
    st.deal_dbg_hist.push(format!("fold-commit nat={:.0}", nat));
    if st.deal_dbg_hist.len() > 12 {
        st.deal_dbg_hist.remove(0);
    }
    schedule_step_locked(st); // gluing pins + re-labels on the next frame
}

// ── sync_card_unfold: scroll-coupled fold/deal + deck gluing ──────
/// The top card of the queue tucks into the deck when scrolling down;
/// the deck-top card peels out into the queue when scrolling up.
/// Both are one-card-per-frame, scroll-direction gated. The CSS
/// keyframe animations bridge the gap between the pinned deck
/// position and the card's flow position so the motion is smooth.
fn sync_unfold(st: &mut PileState) {
    let Some(tr) = &st.transcript else { return };
    let s_top = tr.scroll_top() as f64;
    // True user delta: `last_stop` is synced by every programmatic
    // scroll (park, correction, re-park timers), so this measures
    // only user motion since the last observed position.
    let delta = s_top - st.last_stop;
    st.last_stop = s_top;
    st.deal_dbg_hist.push(format!(
        "Δ={:.0} s_top={:.0}",
        delta, s_top
    ));
    if st.deal_dbg_hist.len() > 12 {
        st.deal_dbg_hist.remove(0);
    }

    let range = tr.scroll_height() as f64 - tr.client_height() as f64;
    let d = range - s_top;

    // Auto-release a click-expanded pile once the user scrolls back
    // down to the bottom. A round that still fits stays expanded.
    let collapse_all = st.pile_open && range > 8.0 && d <= 4.0 && delta > 0.5;
    if collapse_all {
        // Field-level write: `tr` (the `st.transcript` borrow) is
        // still live for the correction pass below, so passing the
        // whole struct to set_pile_open_flag would not compile.
        st.pile_open = false;
        if st.state.pile_open.get_untracked() {
            st.state.pile_open.set(false);
        }
    }

    let mut y = PILE_TOP - s_top; // first slot top, screen coords
    let mut batch: i32 = 0;
    let mut changed = false;
    let mut scroll_shift: f64 = 0.0; // applied programmatic scroll shift
    let mut corr_target: Option<f64> = None; // scroll top to correct to
    // Carry the queue-still correction queued by an async fold commit
    // (commit_fold, 260 ms after its dock); compose it with this
    // step's own dock/deal deltas.
    if st.pending_corr.abs() > 0.5 {
        corr_target = Some((s_top + st.pending_corr).max(0.0));
        st.pending_corr = 0.0;
    }
    let mut folded: Vec<(HtmlElement, f64)> = Vec::new();
    // Cards mid-fold (full-height, clipped): pinned at the deck top
    // while their 260 ms commit timer is in flight.
    let mut pending_slots: Vec<(HtmlElement, f64)> = Vec::new();

    let cards = iter_cards(st);

    // ── Pass 1 (forward): fold full cards, collect compact rows ───
    for el in &cards {
        // Stop at the summary card: later cards are hidden and don't
        // participate in the pile geometry.
        if st.current_summary.as_ref() == Some(el) {
            break;
        }
        let cls = el.class_list();
        if cls.contains("hid") {
            continue;
        }
        if el.has_attribute("data-isSum") {
            // An exempt summary: a full card in the flow — never
            // folds, but its height still positions the cards below.
            let h = if el.offset_height() > 0 {
                el.offset_height() as f64
            } else {
                attr_nat_h(el, 52.0)
            };
            y += h + 12.0;
            continue;
        }
        let slot_top = y;
        if cls.contains("compact") {
            folded.push((el.clone(), slot_top));
            y += COMPACT_ROW_H;
        } else {
            // Full card in the queue.
            if cls.contains("folding") {
                // A pending fold is in flight: the card stays full-height
                // in the layout (the queue below stays put) until its
                // 260 ms commit timer fires. If the user scrolled back
                // out of the pile zone, cancel the fold — the card just
                // stays in the queue (its commit timer no-ops on the
                // missing class).
                if collapse_all {
                    // Pile closed by click: commit the fold now.
                    let nat = attr_nat_h(el, COMPACT_ROW_H);
                    add_compact(el);
                    let drift = nat + FULL_ROW_MARGIN - COMPACT_ROW_H;
                    if drift.abs() > 0.5 {
                        corr_target = Some(corr_target.unwrap_or(s_top) - drift);
                    }
                    folded.push((el.clone(), slot_top));
                    y += COMPACT_ROW_H;
                    changed = true;
                } else if slot_top > PILE_TOP + COMPACT_ROW_H + PILE_BAND {
                    let _ = cls.remove_1("folding");
                    let _ = cls.remove_1("fold-anim");
                    clear_deck(el); // drop the pin (deck-layer/--deck-dy/z)
                    st.deal_dbg_hist.push(format!("fold-cancel slot={:.0}", slot_top));
                    if st.deal_dbg_hist.len() > 12 {
                        st.deal_dbg_hist.remove(0);
                    }
                    y += attr_nat_h(el, COMPACT_ROW_H) + FULL_ROW_MARGIN;
                } else {
                    // Still in the pile zone: keep pinning at the deck
                    // top so the card stays covered while scrolling.
                    pending_slots.push((el.clone(), slot_top));
                    y += attr_nat_h(el, COMPACT_ROW_H) + FULL_ROW_MARGIN;
                }
            } else if collapse_all || (delta > 0.5 && !changed && slot_top <= PILE_TOP + COMPACT_ROW_H) {
                let nat = el.offset_height() as f64;
                let _ = el.set_attribute("data-natH", &nat.to_string());
                if collapse_all {
                    // Pile closed by click: commit instantly, no fold.
                    add_compact(el);
                    let drift = nat + FULL_ROW_MARGIN - COMPACT_ROW_H;
                    if drift.abs() > 0.5 {
                        corr_target = Some(corr_target.unwrap_or(s_top) - drift);
                    }
                    folded.push((el.clone(), slot_top));
                    y += COMPACT_ROW_H;
                    changed = true;
                } else {
                    // Pending fold: the card stays full-height in the
                    // layout while its clip tucks the body up to the
                    // 52 px face; it reads as the card covering the
                    // deck top. A 260 ms timer commits the compact
                    // layout + the queue-still correction (no-op if
                    // the fold was cancelled in the meantime).
                    let c2 = el.class_list();
                    let _ = c2.remove_1("unfold-anim");
                    let _ = el.offset_width(); // reflow
                    let _ = c2.add_1("folding");
                    let _ = c2.add_1("fold-anim");
                    // The pin jumps from the flow position (no
                    // transform) to the r=0 slot (deck top). Animate
                    // that jump like a layer-shift so the card SLIDES
                    // up into the stack top instead of teleporting:
                    // --deck-dy-prev = 0px (the pre-pin transform) and
                    // deck-shift slides 0px -> the live --deck-dy
                    // (the gluing sets it later this same frame). The
                    // 64px fold-in clip runs in parallel via the
                    // .folding.deck-shift composite CSS rule.
                    let _ = el.style().set_property("--deck-dy-prev", "0px");
                    let _ = c2.remove_1("deck-shift");
                    let _ = el.offset_width(); // restart the animation
                    let _ = c2.add_1("deck-shift");
                    st.deal_dbg_hist.push(format!(
                        "dock-pending nat={:.0} slot={:.0} s_top={:.0}",
                        nat, slot_top, s_top
                    ));
                    if st.deal_dbg_hist.len() > 12 {
                        st.deal_dbg_hist.remove(0);
                    }
                    let el2 = el.clone();
                    let to = gloo_timers::callback::Timeout::new(260, move || {
                        with_pile(|cell| {
                            let s = cell.borrow();
                            if let Some(s) = s.as_ref() {
                                let mut st = s.borrow_mut();
                                commit_fold(&mut st, &el2);
                            }
                        });
                    });
                    LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
                    pending_slots.push((el.clone(), slot_top));
                    // Still full-height in the layout until commit: the
                    // slot walk uses the natural height, and the gluing
                    // pins the card at the deck top meanwhile.
                    y += attr_nat_h(el, COMPACT_ROW_H) + FULL_ROW_MARGIN;
                    changed = true;
                }
            } else {
                y += attr_nat_h(el, COMPACT_ROW_H) + FULL_ROW_MARGIN;
            }
        }
    }

    // ── Pass 2 (reverse): deal the newest compact card(s) ─────────
    // The deck-top is the newest compact row. Scrolling up peels it
    // off into the queue, one card per frame. In drain mode
    // (pile-open click or at the very top) all cards deal at once
    // with a stagger.
    // The threshold mirrors the dock threshold (pass 1): a full card
    // folds in when its top edge reaches the deck's BOTTOM edge
    // (PILE_TOP + COMPACT_ROW_H) while scrolling down; it peels back
    // out when its flow slot returns PAST that edge by PILE_BAND
    // while scrolling up. The 12 px band (80 down / 92 up) is the
    // hysteresis that keeps the round trip from flickering.
    let drain = st.pile_open || s_top <= 1.0;
    let n = folded.len();
    if n > 0 {
        let deal_idxs: Vec<usize> = if drain {
            (0..n).collect()
        } else {
            // Only the newest card (index n-1) can deal in scroll mode.
            let newest = n - 1;
            let slot_newest = folded[newest].1;
            let threshold = PILE_TOP + COMPACT_ROW_H + PILE_BAND;
            let deal = delta < -0.5
                && !changed
                && slot_newest >= threshold;
            st.deal_dbg_hist.push(format!(
                "delta={:.1} changed={} slot={:.0} thr={:.0} deal={} drain=false s_top={:.0}",
                delta, changed, slot_newest, threshold, deal, s_top
            ));
            if st.deal_dbg_hist.len() > 8 {
                st.deal_dbg_hist.remove(0);
            }
            if deal {
                vec![newest]
            } else {
                Vec::new()
            }
        };

        // Process newest-first so the deck-top goes first.
        for idx in deal_idxs.iter().rev() {
            let (el, slot_top_ref) = &folded[*idx];
            let slot_top = *slot_top_ref;
            let from_end = n - 1 - idx; // 0 = newest
            let cls = el.class_list();

            let _ = cls.remove_1("compact");
            let _ = cls.remove_1("fold-anim");
            clear_deck(el);
            clear_inline(el);

            // Re-measure: the card may have grown (details toggles).
            let nat = el.offset_height() as f64;
            let _ = el.set_attribute("data-natH", &nat.to_string());

            // Queue-still coupling: expanding this card pushes the
            // content below down by (nat + FULL_ROW_MARGIN - COMPACT_ROW_H);
            // in the scroll-driven single-deal case record a matching
            // scroll shift (applied after pass 2) so the queue stays put
            // while the card settles into its slot. Drain (click-open /
            // at the very top) uses the top edge as its stable reference
            // — no correction there.
            let drift = nat + FULL_ROW_MARGIN - COMPACT_ROW_H;
            let corr = if !drain && drift.abs() > 0.5 {
                corr_target = Some(corr_target.unwrap_or(s_top) + drift);
                drift
            } else {
                0.0
            };

            // The card was pinned at deck layer `from_end`; the
            // unfold animation starts at that pin and ends at the
            // (corrected) flow slot, so it reads as the deck's front
            // face sliding down out of the pile into the queue.
            // (Inverted stack: layer `from_end` sits ABOVE the newest
            // face, at PILE_TOP - from_end*PEEK.)
            let deal_dy = if from_end < DECK_SHOW {
                PILE_TOP - from_end as f64 * DECK_PEEK - (slot_top - corr)
            } else {
                0.0
            };
            let _ = el.style().set_property("--deal-dy", &format!("{deal_dy:.0}px"));

            let delay_ms = (batch * 50).min(250);
            let _ = el.style().set_property("animation-delay", &format!("{delay_ms}ms"));
            let _ = el.offset_width(); // reflow
            let _ = el.class_list().add_1("unfold-anim");
            // Mirror of the fold side (`.folding` z 51): the
            // `unfold-anim` class carries z 51 (above the deck's 50)
            // for the whole slide-out, so the card reads as the top
            // card sliding down in FRONT of the pile, revealing the
            // promoted face behind it — the exact reverse of the
            // dock's slide-up. (z must live on the class, not inline:
            // the gluing cleanup calls clear_deck on cards that left
            // the deck, which wipes inline z-index.) Once the slide
            // has finished the card is fully in the queue (its top
            // edge is past the deck's bottom edge), so the release
            // timer drops the class and z falls back to the queue
            // layering — invisible, and self-healing if a re-dock
            // happens in the meantime.
            let el3 = el.clone();
            let release_ms = 300u32 + delay_ms.max(0) as u32;
            let to = gloo_timers::callback::Timeout::new(release_ms, move || {
                let node: web_sys::Node = el3.clone().unchecked_into();
                if node.is_connected() {
                    let _ = el3.class_list().remove_1("unfold-anim");
                }
            });
            LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
            batch += 1;
            changed = true;
        }
    }

    // ── Apply the recorded scroll correction (queue stillness) ─────
    // A dock shrinks the content above the queue; a deal grows it.
    // Shifting the viewport by the same delta keeps the queue STILL.
    // Done after pass 2 so all card mutations are in; the gluing
    // below and the next frame's delta account for the applied shift.
    if let Some(target) = corr_target {
        let _ = tr.style().set_property("scroll-behavior", "auto");
        let _ = tr.set_scroll_top(target.max(0.0) as i32);
        let applied = tr.scroll_top() as f64;
        st.last_stop = applied;
        scroll_shift = applied - s_top;
    }

    // ── Deck gluing: pin the newest compact rows at the top ───────
    // The deck is the newest few folded cards, pinned at the
    // transcript top via the --deck-dy custom property (CSS rule on
    // .deck-layer). The transform is recomputed every frame so the
    // deck tracks the scroll exactly.
    let compact: Vec<&(HtmlElement, f64)> = folded
        .iter()
        .filter(|(el, _)| el.class_list().contains("compact"))
        .collect();
    let show = if st.pile_open { 0 } else { compact.len().min(DECK_SHOW) };
    // Previous frame's pinned layers (el, r): used to detect a layer
    // shift (this row's r changed) and to release the pin of cards that
    // left the deck this frame.
    let prev_deck = std::mem::take(&mut st.deck_layers);
    let mut new_deck: Vec<(HtmlElement, i32)> = Vec::new();
    for r in 0..show {
        // r=0 is the newest folded row: the readable face, the LOWEST
        // of the inverted cascade. Older layers (r>0) sit ABOVE it,
        // their top edges peeking up by DECK_PEEK each (old cards on
        // top, layer edges above the stack).
        let slot_top = compact[compact.len() - 1 - r].1;
        let el = &compact[compact.len() - 1 - r].0;
        let glued_y = PILE_TOP - r as f64 * DECK_PEEK;
        // The correction shifted the viewport by `scroll_shift` this
        // frame; the rows' flow tops moved with it, so the pin
        // offset accounts for the post-correction position.
        let dy = (glued_y - slot_top + scroll_shift).round() as i32;
        // Layer-shift merge: when this row's r index changed since the
        // last frame (a card dealt out, or a new card docked in), slide
        // it from its old pin (--deck-dy-prev) to the new one instead of
        // re-pinning it in a single frame — the new top card must not
        // pop into place, it must glide into the deck-top slot.
        let prev_r = prev_deck
            .iter()
            .find(|(e, _)| same_el(e, el))
            .map(|(_, r)| *r);
        if prev_r != Some(r as i32) {
            let old_opt = el.style().get_property_value("--deck-dy").ok();
            let prev_dy = match old_opt.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => "0px",
            };
            let _ = el.style().set_property("--deck-dy-prev", prev_dy);
            let cls = el.class_list();
            let _ = cls.remove_1("deck-shift");
            let _ = el.offset_width(); // reflow so the animation restarts
            let _ = cls.add_1("deck-shift");
        }
        let _ = el.style().set_property("--deck-dy", &format!("{dy}px"));
        let _ = el.style().set_property("z-index", &format!("{}", (50 - r) as i32));
        let cls = el.class_list();
        let _ = cls.add_1("deck-layer");
        if r == 0 {
            let _ = cls.add_1("deck-top");
        } else {
            let _ = cls.remove_1("deck-top");
        }
        new_deck.push((el.clone(), r as i32));
    }
    // Cards mid-fold (still full-height, clipped): pin each at the
    // r=0 slot (deck top, z 51 — above the deck's 50) so the card
    // stays covered even if the user keeps scrolling during the
    // 260 ms fold window; its commit swaps it into the compact
    // deck-layer set on the next frame.
    for (el, slot_top) in &pending_slots {
        let dy = (PILE_TOP - slot_top + scroll_shift).round() as i32;
        let _ = el.style().set_property("--deck-dy", &format!("{dy}px"));
        let _ = el.style().set_property("z-index", "51");
        let _ = el.class_list().add_1("deck-layer");
        new_deck.push((el.clone(), 0));
    }
    // Clear pinning from cards that left the deck.
    for (el, _) in &prev_deck {
        if !new_deck.iter().any(|(e, _)| same_el(e, el)) {
            clear_deck(el);
        }
    }
    st.deck_layers = new_deck;
}

/// Clear a card's deck pinning (--deck-dy CSS var + z-index + deck classes
/// + the layer-shift animation state).
fn clear_deck(el: &HtmlElement) {
    let s = el.style();
    let _ = s.remove_property("--deck-dy");
    let _ = s.remove_property("--deck-dy-prev");
    let _ = s.remove_property("z-index");
    let cls = el.class_list();
    let _ = cls.remove_1("deck-layer");
    let _ = cls.remove_1("deck-top");
    let _ = cls.remove_1("deck-shift");
}

// ── sync_card_shrink (legacy syncCardShrink, faithful port) ───────
// The bottom-most card straddling the transcript's bottom edge is
// rendered SHORTER (its own border-radius + box-shadow relief is drawn
// at the cut line — the frame stays intact, never sliced by the
// container edge), and the spacer below absorbs the difference so the
// total content height — and therefore the user's scroll position —
// never moves. This restores the bottom card's intact relief that a
// plain overflow clip would otherwise slice off.
fn sync_shrink(st: &mut PileState) {
    let Some(tr) = &st.transcript else { return };
    // Cut line = the content-box bottom (the transcript's border-box
    // bottom minus its bottom padding). Shrinking the straddling card
    // to THIS line leaves the bottom padding empty below the card, so
    // the card's border-radius AND drop-shadow relief stay fully
    // visible — identical to the parked-at-rest look — instead of the
    // shadow being sliced at the container's bottom edge.
    let rect = tr.get_bounding_client_rect();
    let pad_bottom = web_sys::window()
        .and_then(|w| w.get_computed_style(tr).ok())
        .flatten()
        .and_then(|cs| cs.get_property_value("padding-bottom").ok())
        .and_then(|s| s.trim_end_matches("px").parse::<f64>().ok())
        .unwrap_or(0.0);
    let cut = rect.bottom() - pad_bottom;

    // Bottom-most card whose top is above the cut line (it straddles
    // or sits above the transcript's bottom edge).
    let cards = iter_cards(st);
    let mut target: Option<HtmlElement> = None;
    for el in cards.iter().rev() {
        if el.class_list().contains("hid") {
            continue;
        }
        // A card mid-fold is transient; don't let the shrink pass touch it.
        if el.class_list().contains("folding") {
            continue;
        }
        if el.get_bounding_client_rect().top() < cut - 1.0 {
            target = Some(el.clone());
            break;
        }
    }

    // Release a stale shrunken card (target changed).
    if let Some(sc) = st.shrunken_card.clone() {
        let still = target
            .as_ref()
            .map(|t| same_el(t, &sc))
            .unwrap_or(false);
        if !still {
            release_shrink_card(&sc);
            st.shrunken_card = None;
        }
    }

    let Some(target) = target else {
        set_spacer_height(st, 0.0);
        return;
    };

    let top = target.get_bounding_client_rect().top();
    let desired = cut - top;
    if desired <= 0.5 {
        // Fully below the cut: collapse it entirely, spacer compensates.
        let s = target.style();
        let _ = s.set_property("height", "0px");
        let _ = s.set_property("overflow", "hidden");
        let _ = s.set_property("visibility", "hidden");
        set_spacer_height(st, attr_nat_h(&target, 52.0));
        st.shrunken_card = Some(target);
    } else {
        let _ = target.style().set_property("visibility", "");
        let nat = attr_nat_h(&target, 52.0);
        if desired < nat - 0.5 {
            let s = target.style();
            let _ = s.set_property("height", &format!("{desired}px"));
            let _ = s.set_property("overflow", "hidden");
            set_spacer_height(st, nat - desired);
            st.shrunken_card = Some(target);
        } else {
            let s = target.style();
            let _ = s.set_property("height", "");
            let _ = s.set_property("overflow", "");
            set_spacer_height(st, 0.0);
            st.shrunken_card = None;
        }
    }
}

/// DOM node identity (legacy `shrunkenCard !== target`).
fn same_el(a: &HtmlElement, b: &HtmlElement) -> bool {
    a.unchecked_ref::<web_sys::Node>()
        .is_same_node(Some(b.unchecked_ref::<web_sys::Node>()))
}

fn release_shrink(st: &mut PileState) {
    if let Some(c) = st.shrunken_card.take() {
        release_shrink_card(&c);
    }
    set_spacer_height(st, 0.0);
}

fn release_shrink_card(card: &HtmlElement) {
    let s = card.style();
    let _ = s.set_property("display", "");
    let _ = s.set_property("height", "");
    let _ = s.set_property("overflow", "");
    let _ = s.set_property("visibility", "");
}

fn set_spacer_height(st: &PileState, h: f64) {
    if let Some(sp) = &st.scroll_spacer {
        let _ = sp.style().set_property("height", &format!("{h}px"));
    }
}

// ── pile open/close (legacy setPileOpen + transcript click) ───────
fn on_transcript_click(e: &web_sys::Event) {
    with_pile(|cell| {
        let st = cell.borrow();
        let Some(st) = st.as_ref() else { return };
        let Some(w) = web_sys::window() else { return };

        // Text selection is left alone (legacy guard).
        if let Ok(Some(sel)) = w.get_selection() {
            if !sel.is_collapsed() {
                return;
            }
        }
        let Some(target) = e.target() else { return };
        let Ok(el): Result<web_sys::Element, _> = target.clone().dyn_into() else {
            return;
        };
        // <summary> toggles and anything outside a card are ignored.
        if el.closest("summary").ok().flatten().is_some() {
            return;
        }
        let row = el.closest(".event").ok().flatten();
        let Some(row) = row else { return };
        let row: HtmlElement = row.unchecked_into();
        let is_compact = row.class_list().contains("compact");

        let mut st = st.borrow_mut();
        if st.pile_open {
            set_pile_open(&mut st, false);
        } else if is_compact {
            set_pile_open(&mut st, true);
        }
    });
}

/// Set the pile-open flag on both the engine field and the reactive
/// signal the stack header reads.
fn set_pile_open_flag(st: &mut PileState, v: bool) {
    st.pile_open = v;
    if st.state.pile_open.get_untracked() != v {
        st.state.pile_open.set(v);
    }
}

/// Public toggle for the stack header's click (expand / collapse).
pub fn toggle_pile_open() {
    with_pile(|cell| {
        let st = cell.borrow();
        let Some(st) = st.as_ref() else { return };
        let mut st = st.borrow_mut();
        let v = !st.pile_open;
        set_pile_open(&mut st, v);
    });
}

fn set_pile_open(st: &mut PileState, v: bool) {
    if st.pile_open == v {
        return;
    }
    set_pile_open_flag(st, v);
    if !v {
        // Re-fold everything at once: a round that fits the viewport
        // has no scroll range to dock cards back one by one.
        for el in iter_cards(st) {
            let cls = el.class_list();
            if cls.contains("hid") || el.has_attribute("data-isSum") {
                continue;
            }
            if st.current_summary.as_ref() == Some(&el) {
                continue;
            }
            if !cls.contains("compact") {
                add_compact(&el);
            }
        }
    }
    sync_unfold(st);
    sync_shrink(st);
}

// ── input gutter (legacy syncInputGutter) ─────────────────────────
/// The input module must line up with the message cards, which sit
/// inside the transcript's 16px padding. A classic scrollbar eats
/// width from the content box, so the module's right margin
/// compensates by the measured gutter.
pub fn sync_input_gutter() {
    with_pile(|cell| {
        let st = cell.borrow();
        let Some(st) = st.as_ref() else { return };
        let st = st.borrow();
        if let (Some(tr), Some(im)) = (&st.transcript, &st.input_module) {
            let gutter = (tr.offset_width() - tr.client_width()).max(0) as i32;
            let _ = im.style().set_property("margin-right", &format!("{}px", 16 + gutter));
        }
    });
}

// ── visualViewport keyboard lift (legacy onVV listener) ───────────
fn on_vv_event() {
    with_pile(|cell| {
        let st = cell.borrow();
        let Some(st) = st.as_ref() else { return };
        let Some(w) = web_sys::window() else { return };
        let Some(vv) = w.visual_viewport() else { return };
        let h = vv.height();
        if h <= 0.0 {
            return;
        }
        let inner = w.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let open = h < inner * 0.85;
        let mut st = st.borrow_mut();
        if open && !st.kb_open {
            st.kb_open = true;
            // WeChat/Telegram-style: if the user is near the bottom,
            // keep the newest messages in view once the lift settles.
            if let Some(t) = &st.transcript {
                let near = t.scroll_height() - t.scroll_top() - t.client_height() < 140;
                if near {
                    let t2 = t.clone();
                    // The Timeout must stay alive or it clears itself
                    // (legacy `setTimeout` had the same semantics).
                    let to = gloo_timers::callback::Timeout::new(320, move || {
                        let _ = t2.set_scroll_top(t2.scroll_height());
                        with_pile(|cell| {
                            let s = cell.borrow();
                            if let Some(s) = s.as_ref() {
                                s.borrow_mut().last_stop = t2.scroll_top() as f64;
                            }
                        });
                    });
                    LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
                }
            }
            sync_shrink(&mut st);
        } else if !open {
            st.kb_open = false;
        }
        // iOS can auto-scroll the document to reveal the focused
        // input; pin the document top so the app chrome never jumps.
        if let Some(doc) = w.document() {
            if let Some(de) = doc.document_element() {
                if de.scroll_top() != 0 {
                    de.set_scroll_top(0);
                }
            }
            if let Some(body) = doc.body() {
                let _ = body.style().set_property("height", &format!("{h:.0}px"));
            }
        }
        sync_shrink(&mut st);
    });
}

// silence unused-import noise on non-wasm builds
#[allow(dead_code)]
fn _types() -> (DomRectReadOnly, EventTarget, Selection, web_sys::Event) {
    unreachable!()
}
