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
///
/// MUST EQUAL `#transcript`'s CSS padding-top (28px in style.css):
/// the walk starts at `PILE_TOP - s_top` and the gluing target is
/// `PILE_TOP - r*DECK_PEEK`, so the constant cancels out of the pin
/// arithmetic — the face actually pins at the transcript's padding.
/// (Change both together or the deck face jumps.)
const PILE_TOP: f64 = 28.0;
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
/// newest, i.e. PILE_TOP-15 = 13px from the transcript top — within
/// the transcript's top padding, nothing clips).
const DECK_PEEK: f64 = 5.0;
/// Margin-bottom of a full (queue) row: the base .event margin.
/// Compact rows are flush (0 margin; pitch = COMPACT_ROW_H). The
/// queue-still drift when a card folds: nat + FULL_ROW_MARGIN - COMPACT_ROW_H.
const FULL_ROW_MARGIN: f64 = 12.0;

/// Velocity-matched in/out animation duration (ms).
///
/// The fold/deal slide durations track the user's wheel speed: a fast
/// fling gets a short, snappy motion; a slow scroll a long, gentle
/// one — so the card in/out speed stays coupled to the scroll speed
/// (fixed wall-clock durations read as "same speed at any scroll
/// speed": fast against a fast fling, snappy against a slow crawl).
/// `vel` is px/ms of user scroll; 0/unknown (a click, idle rAF, or
/// the very first step) falls back to the classic 220 ms.
fn inout_dur_ms(vel: f64) -> f64 {
    if vel < 0.5 {
        return 220.0;
    }
    // ~6 px/ms (a brisk wheel flick) settles in 150 ms; clamp to keep
    // the motion inside a snappy..gentle window.
    (900.0 / vel).clamp(80.0, 340.0)
}

// ── Phase C: critically-damped slide springs ───────────────────────
/// One card's in/out slide, integrated per rAF frame instead of a
/// fixed-duration CSS keyframe. `x` is the element's `--deck-dy`
/// value (transform space, px); the target is recomputed EVERY frame
/// from live layout (its deck slot for pinned layers, 0 / natural
/// flow for dealt and leaving cards), so a mid-flight scroll or
/// re-dock retargets the spring smoothly instead of restarting it.
/// `vel` is px/ms; on release the card's scroll-coupled motion is
/// carried by the live layout term, so the spring starts from rest
/// and only settles the residual — the velocity handoff that keeps
/// interrupted motion continuous (iOS notification-center feel).
#[derive(Clone)]
struct CardSpring {
    el: HtmlElement,
    /// true: target is the element's deck slot (pinned layer, handled
    /// by the gluing loops); false: target is 0 / natural flow
    /// (dealt + leaving cards, handled by `step_flow_springs`).
    to_slot: bool,
    x: f64,
    vel: f64,
    /// Stiffness rad/ms (critical damping: c = 2*sqrt(k) with m = 1).
    omega: f64,
    /// No integration before this perf-ms (drain stagger).
    delay_until: f64,
    /// perf-ms of the last integration step.
    t_prev: f64,
}

/// Stiffness that settles a critically damped step response in
/// `dur` ms (≈98% at t = 5.3/ω). Driven by the same velocity-matched
/// mapping as the old CSS durations: fast flings settle fast, slow
/// crawls settle gently.
fn spring_omega(vel: f64) -> f64 {
    5.3 / inout_dur_ms(vel)
}

/// Monotonic perf-ms clock (0.0 when the Performance API is
/// unavailable — every caller must treat 0.0 as "time unknown").
fn perf_now() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

fn spring_idx(st: &PileState, el: &HtmlElement) -> Option<usize> {
    st.springs.iter().position(|s| same_el(&s.el, el))
}

fn spring_drop(st: &mut PileState, el: &HtmlElement) {
    st.springs.retain(|s| !same_el(&s.el, el));
}

/// Adopt (or replace) a spring for `el`. `x0` is the starting
/// `--deck-dy` value; the per-frame target comes from the gluing
/// loops (slot) or is 0 (flow). `delay_ms` staggers drain batches.
fn spring_adopt(
    st: &mut PileState,
    el: &HtmlElement,
    to_slot: bool,
    x0: f64,
    delay_ms: u32,
    now_ms: f64,
    omega: f64,
) {
    spring_drop(st, el);
    st.springs.push(CardSpring {
        el: el.clone(),
        to_slot,
        x: x0,
        vel: 0.0,
        omega,
        delay_until: now_ms + delay_ms as f64,
        t_prev: now_ms,
    });
    if st.springs.len() > 48 {
        st.springs.drain(0..st.springs.len() - 48);
    }
}

/// One critically-damped spring step toward `target`. Returns true
/// when settled (caller snaps `x` to `target` and retires the
/// spring).
fn spring_step(sp: &mut CardSpring, target: f64, now_ms: f64) -> bool {
    // Exact closed-form step of the critically-damped oscillator
    // (x - target = (dx + B*t)*e^(-w*t), B = dv + w*dx): unconditionally
    // stable and monotone — no stability cliff when a stiff spring
    // (fast scroll) meets a long frame. (Semi-implicit Euler was
    // unstable for omega*dt above ~0.83: observed x blowing up to
    // ±1e14.) Cap the step to 500 ms so a huge timer gap never
    // instantaneously teleports the value.
    let dt = (now_ms - sp.t_prev).clamp(0.0, 500.0);
    sp.t_prev = now_ms;
    let w = sp.omega;
    if dt > 0.0 {
        let dx = sp.x - target;
        let dv = sp.vel;
        // x(t) - target = (dx + b*t)*e^(-w*t),  b = dv + w*dx
        // v(t)          = (dv - w*b*t)*e^(-w*t)
        let b = dv + w * dx;
        let e = (-w * dt).exp();
        sp.x = target + (dx + b * dt) * e;
        sp.vel = (dv - w * b * dt) * e;
    }
    (sp.x - target).abs() < 0.5 && sp.vel.abs() < 0.05
}

/// Advance every FLOW-targeted spring (dealt / leaving edges) and
/// write its per-frame `--deck-dy`; pinned layers' springs are
/// advanced by the gluing loops, which own those elements' writes.
/// A settled spring is retired and the element's transform released.
fn step_flow_springs(st: &mut PileState, now_ms: f64) {
    let mut i = 0;
    while i < st.springs.len() {
        let is_flow = !st.springs[i].to_slot;
        let el = st.springs[i].el.clone();
        if is_flow {
            let node: web_sys::Node = el.clone().unchecked_into();
            // Retired when detached or re-folded back into the deck
            // (the gluing loop re-pins it, so the flow spring is stale).
            let alive = node.is_connected() && !el.class_list().contains("compact");
            if !alive {
                // Retired (detached, or re-folded into the deck before
                // the glide settled): drop the glide class too, so it
                // can't stick on the card's next state.
                let _ = el.class_list().remove_1("deck-gliding");
                st.springs.remove(i);
                continue;
            }
            let mut sp = st.springs[i].clone();
            if now_ms < sp.delay_until {
                sp.t_prev = now_ms; // stagger hold: rest at x0
                st.springs[i] = sp;
            } else {
                let settled = spring_step(&mut sp, 0.0, now_ms);
                st.springs[i] = sp;
                if settled {
                    // Rested exactly at the natural flow position:
                    // release the inline pin state so the card is a
                    // plain queue card again (the old release timer's
                    // job, now driven by the spring settling).
                    let _ = el.class_list().remove_1("unfold-anim");
                    let _ = el.class_list().remove_1("deck-gliding");
                    clear_inline(&el);
                    st.springs.remove(i);
                    continue;
                }
            }
            let _ = el
                .style()
                .set_property("--deck-dy", &format!("{:.1}px", st.springs[i].x));
        }
        i += 1;
    }
}

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
    /// Wall time (ms) of the previous full step: feeds the scroll
    /// velocity estimate that drives the velocity-matched in/out
    /// durations (see `inout_dur_ms`).
    last_step_t: f64,
    /// Wall time (ms) of the last user scroll motion (delta > 0.5):
    /// the commit stillness gate defers fold commits that would land
    /// mid-gesture.
    last_user_scroll_t: f64,
    /// Phase B: pending folds awaiting the settle-driven commit
    /// ((card, perf-ms when it entered the dock)).
    fold_watch: Vec<(HtmlElement, f64)>,
    /// Phase C: per-element critically-damped slide springs that
    /// replace the fixed-duration CSS keyframe in/out slides.
    springs: Vec<CardSpring>,
    /// prefers-reduced-motion: skip springs/glides, snap instead.
    reduced_motion: bool,
    // Closures kept alive for the app lifetime (wasm GC):
    raf: Option<Closure<dyn Fn()>>,
    scroll_cb: Option<Closure<dyn Fn()>>,
    click_cb: Option<Closure<dyn Fn(web_sys::Event)>>,
    animend_cb: Option<Closure<dyn Fn(web_sys::Event)>>,
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
            last_step_t: 0.0,
            last_user_scroll_t: 0.0,
            fold_watch: Vec::new(),
            springs: Vec::new(),
            reduced_motion: false,
            raf: None,
            scroll_cb: None,
            click_cb: None,
            animend_cb: None,
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
        // Phase C: under prefers-reduced-motion the slides are snaps,
        // not glides — skip spring adoption everywhere.
        ps.reduced_motion = web_sys::window()
            .and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok())
            .flatten()
            .map(|mq| mq.matches())
            .unwrap_or(false);

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

        // animationend (delegated on the transcript): drop the `.enter`
        // mount-fade class when a card's fadeIn completes. The event
        // also fires for the deck/unfold animations, but those cards
        // shed `.enter` long ago, so this is a no-op for them. Under
        // reduced motion the animation never runs and the class harmlessly
        // stays put (its rule is disabled there too).
        if let Some(t) = &ps.transcript {
            let vt: EventTarget = t.clone().unchecked_into();
            let on_animend = Closure::<dyn Fn(web_sys::Event)>::new(|e: web_sys::Event| {
                let Some(target) = e.target() else {
                    return;
                };
                let el2: web_sys::Element = target.unchecked_into();
                let _ = el2.class_list().remove_1("enter");
            });
            let _ = vt.add_event_listener_with_callback("animationend", on_animend.as_js_value().unchecked_ref::<js_sys::Function>());
            ps.animend_cb = Some(on_animend);
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
        // Spring / commit bookkeeping belongs to the previous
        // session's elements: drop it (stale entries would be
        // retired lazily, but why keep them alive).
        st.springs.clear();
        st.fold_watch.clear();
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
    // Velocity-matched in/out duration (a dealt card re-joining the
    // deck must use the default duration for layer cascades, not a
    // stale speed-matched one).
    let _ = s.remove_property("--inout-dur");
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

/// Re-pin a deck layer to its new r-slot, starting from where it
/// VISUALLY WAS before the commit. Phase C: instead of a CSS keyframe
/// (the old `deck-shift`), the layer's `--deck-dy` is driven by a
/// critically damped spring that the gluing loop integrates per frame.
///
/// The commit's queue-still correction scroll moves the layout (and
/// therefore the stale transform's render point) by `shift` px in the
/// same task, so the spring starts from `old_dy + shift` — the
/// pre-commit render position — and glides to the live slot target,
/// which the gluing loop recomputes every frame (a mid-gesture scroll
/// retargets the spring instead of restarting an animation).
fn repin_layer(st: &mut PileState, le: &HtmlElement, r: i32, tr_top: f64, shift: f64, now_ms: f64) {
    let s = le.style();
    let old_dy = s
        .get_property_value("--deck-dy")
        .ok()
        .as_deref()
        .and_then(|v| v.trim_end_matches("px").trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let target = PILE_TOP - r as f64 * DECK_PEEK;
    // `old_dy + shift` renders exactly at the pre-correction position
    // (same convention as the old `--deck-dy-prev` keyframe `from`).
    let start_dy = old_dy + shift;
    let cls = le.class_list();
    let _ = cls.add_1("deck-layer");
    let _ = s.set_property("z-index", &format!("{}", 50 - r));
    if r == 0 {
        let _ = cls.add_1("deck-top");
    } else {
        let _ = cls.remove_1("deck-top");
    }
    if !st.reduced_motion {
        spring_adopt(st, le, true, start_dy, 0, now_ms, spring_omega(0.0));
        // Paint the start value now; the gluing loop takes over the
        // spring on its next frame.
        let _ = s.set_property("--deck-dy", &format!("{start_dy:.1}px"));
    } else {
        spring_drop(st, le);
        // Snap straight to the new slot (reduced motion).
        let screen_rel = le.get_bounding_client_rect().top() - tr_top;
        let new_dy = old_dy - screen_rel + target;
        let _ = s.set_property("--deck-dy", &format!("{new_dy:.0}px"));
    }
}

/// Phase B: the settle-driven commit. Re-checks every 80 ms: the
/// pending fold commits once the user has been STILL ≥ 150 ms AND the
/// clip has had its (velocity-matched) duration to play out; a 2.5 s
/// cap keeps a runaway gesture from holding the fold forever. No
/// blind timer — the card stays pinned (clipped + glued + spring) as
/// long as the user keeps scrolling, and the commit lands at the
/// settle point instead. No-op when the fold was cancelled in the
/// meantime (scrolled back out of the pile zone: the `folding` class
/// is gone) — the card just stays in the queue.
fn watch_fold_commit(st: &mut PileState, el: &HtmlElement, clip_ms: f64) {
    let cls = el.class_list();
    if !cls.contains("folding") {
        st.fold_watch.retain(|(e, _)| !same_el(e, el));
        return; // cancelled — the card just stays in the queue
    }
    let node: web_sys::Node = el.clone().unchecked_into();
    if !node.is_connected() {
        st.fold_watch.retain(|(e, _)| !same_el(e, el));
        return;
    }
    let now_ms = perf_now();
    let t0 = st.fold_watch
        .iter()
        .find(|(e, _)| same_el(e, el))
        .map(|(_, t)| *t)
        .unwrap_or(0.0);
    let idle_ok = st.last_user_scroll_t == 0.0 || now_ms - st.last_user_scroll_t >= 150.0;
    let elapsed = if t0 > 0.0 { now_ms - t0 } else { f64::INFINITY };
    if (idle_ok && elapsed >= clip_ms) || elapsed >= 2500.0 {
        commit_fold(st, el);
        return; // commit_fold retires the watch entry
    }
    let el2 = el.clone();
    let to = gloo_timers::callback::Timeout::new(80, move || {
        with_pile(|cell| {
            let s = cell.borrow();
            if let Some(s) = s.as_ref() {
                let mut st = s.borrow_mut();
                watch_fold_commit(&mut st, &el2, clip_ms);
            }
        });
    });
    LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
}

/// Commit a pending fold: the card swaps to the compact layout. The
/// queue-still correction and the deck re-pin run SYNCHRONOUSLY, in
/// this same task as the layout shrink, so the browser only ever
/// paints the final state (the old cross-frame `pending_corr` path
/// let an intermediate state paint for a frame — the fold-side queue
/// jump / stale deck flicker). No-op when the fold was cancelled
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
    // ── stillness gate ─────────────────────────────────────────────
    // The queue-still correction scroll below lands in the SAME task
    // as the layout shrink. If the user is mid-gesture, that program-
    // matic scrollTop change fights their scroll and the deck visibly
    // jolts. The card is perfectly safe to hold in its pending state
    // (clipped + glued by the gluing loop) until the scroll settles:
    // reschedule the commit 80 ms later and re-check.
    let now_ms = web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0);
    if now_ms > 0.0 && st.last_user_scroll_t > 0.0 && now_ms - st.last_user_scroll_t < 150.0 {
        st.deal_dbg_hist.push(format!(
            "commit-defer age={:.0}",
            now_ms - st.last_user_scroll_t
        ));
        let el2 = el.clone();
        let to = gloo_timers::callback::Timeout::new(80, move || {
            with_pile(|cell| {
                let s = cell.borrow();
                if let Some(s) = s.as_ref() {
                    let mut st = s.borrow_mut();
                    commit_fold(&mut st, &el2);
                }
            });
        });
        LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
        return;
    }
    let nat = attr_nat_h(el, 52.0);
    add_compact(el); // compact + removes folding/fold-anim + clears inline

    let tr = st.transcript.clone();
    let mut commit_shift = 0.0;
    if let Some(tr) = &tr {
        // ── queue-still correction, same task as the layout shrink ─
        // Compacting shrank the content above the queue by
        // (nat + FULL_ROW_MARGIN - COMPACT_ROW_H); scroll by the same
        // delta so the queue stays still.
        let drift = nat + FULL_ROW_MARGIN - COMPACT_ROW_H;
        let s_b = tr.scroll_top() as f64;
        if drift.abs() > 0.5 {
            let _ = tr.style().set_property("scroll-behavior", "auto");
            let _ = tr.set_scroll_top(((s_b - drift).max(0.0)) as i32);
        }
        let s_a = tr.scroll_top() as f64;
        st.last_stop = s_a;
        // Applied scroll delta. repin_layer needs it: the correction
        // moved the layout, so a layer's stale transform now renders
        // `|shift|` px off its pre-commit position; repin adds it
        // back into the animation's `from` keyframe (see repin_layer).
        let shift = s_a - s_b;
        commit_shift = shift;

        // ── synchronous deck re-pin ──────────────────────────────
        // The correction moved the viewport, so every pinned layer
        // shifted on screen. Re-pin now (not on the next rAF) so no
        // stale pin paints: the committing card becomes the new r=0
        // face; each older layer demotes one slot and slides from its
        // pre-commit position; the oldest visible layer drops off.
        let tr_top = tr.get_bounding_client_rect().top();
        let now_ms = perf_now();
        let prev_deck = std::mem::take(&mut st.deck_layers);
        let mut new_deck: Vec<(HtmlElement, i32)> = Vec::new();
        for (le, r) in &prev_deck {
            if same_el(le, el) {
                continue; // the committing card, pinned below as r=0
            }
            let new_r = r + 1;
            if new_r >= DECK_SHOW as i32 {
                // Pushed off the deck by the new face: glide it back
                // to its flow position and fade the edge out while
                // the promoted layers cascade behind it (an instant
                // clear would leave a one-frame dip in the stack's
                // top silhouette). Not kept in st.deck_layers — the
                // gluing loop never re-pins it; leave_deck's spring +
                // timer release it.
                leave_deck(st, le, now_ms);
                continue;
            }
            repin_layer(st, le, new_r, tr_top, shift, now_ms);
            new_deck.push((le.clone(), new_r));
        }
        repin_layer(st, el, 0, tr_top, shift, now_ms);
        new_deck.push((el.clone(), 0));
        st.deck_layers = new_deck;
        // This commit is driven by the settle watcher; retire the
        // card's watch entry (a collapse_all commit has none).
        st.fold_watch.retain(|(e, _)| !same_el(e, el));
    }

    st.deal_dbg_hist.push(format!("fold-commit nat={:.0} shift={:.0}", nat, commit_shift));
    if st.deal_dbg_hist.len() > 12 {
        st.deal_dbg_hist.remove(0);
    }
    schedule_step_locked(st);
}

// ── sync_card_unfold: scroll-coupled fold/deal + deck gluing ──────
/// The top card of the queue tucks into the deck when scrolling down;
/// the deck-top card peels out into the queue when scrolling up.
/// Both are one-card-per-frame, scroll-direction gated. The CSS
/// keyframe animations bridge the gap between the pinned deck
/// position and the card's flow position so the motion is smooth.
fn sync_unfold(st: &mut PileState) {
    // Block-scoped transcript access: the spring bookkeeping in the
    // passes below needs `st` mutable, which a live `&st.transcript`
    // borrow would forbid.
    let (s_top, range) = {
        let Some(tr) = &st.transcript else { return };
        let s_top = tr.scroll_top() as f64;
        let range = tr.scroll_height() as f64 - tr.client_height() as f64;
        (s_top, range)
    };
    // True user delta: `last_stop` is synced by every programmatic
    // scroll (park, correction, re-park timers), so this measures
    // only user motion since the last observed position.
    let delta = s_top - st.last_stop;
    st.last_stop = s_top;
    // User scroll speed for this step (px/ms): feeds the
    // velocity-matched in/out durations (`inout_dur_ms`). A step after
    // a long idle clamps dt so stale motion reads as slow, not fast.
    // Falls back to a nominal 16 ms frame if the Performance API is
    // unavailable.
    let now_t = web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0);
    let dt_ms = if st.last_step_t > 0.0 && now_t > 0.0 {
        (now_t - st.last_step_t).clamp(8.0, 100.0)
    } else {
        16.0
    };
    st.last_step_t = now_t;
    let scroll_vel = if delta.abs() > 0.5 {
        // Real user scroll motion: stamp it so the commit stillness
        // gate (commit_fold) can defer commits mid-gesture.
        st.last_user_scroll_t = now_t;
        delta.abs() / dt_ms
    } else {
        0.0
    };
    st.deal_dbg_hist.push(format!(
        "Δ={:.0} s_top={:.0}",
        delta, s_top
    ));
    if st.deal_dbg_hist.len() > 12 {
        st.deal_dbg_hist.remove(0);
    }

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
                    if !st.reduced_motion {
                        // Glide the card back into the queue instead of
                        // teleporting: a Flow spring carries its current
                        // pin offset down to 0 while the deck-gliding
                        // class binds --deck-dy to the transform.
                        let old_dy = el.style()
                            .get_property_value("--deck-dy")
                            .ok()
                            .and_then(|v| v.trim_end_matches("px").trim().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        spring_adopt(st, el, false, old_dy, 0, now_t, spring_omega(scroll_vel));
                        let _ = cls.remove_1("deck-layer");
                        let _ = cls.add_1("deck-gliding");
                    } else {
                        spring_drop(st, el);
                        clear_deck(el); // drop the pin (deck-layer/--deck-dy/z)
                    }
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
                    // deck top. Phase B: the commit is SETTLE-DRIVEN —
                    // no blind timer. The settle watcher
                    // (watch_fold_commit) holds the card pinned while
                    // the user keeps scrolling, and commits ~150 ms
                    // after the last motion (capped at 2.5 s).
                    let c2 = el.class_list();
                    let _ = c2.remove_1("unfold-anim");
                    // A cancelled fold's glide (deck-gliding) is over:
                    // re-docking re-pins the card, so drop the glide
                    // class now or it sticks on the compact card.
                    let _ = c2.remove_1("deck-gliding");
                    let _ = el.offset_width(); // reflow
                    // Velocity-matched in/out: the tuck duration
                    // scales inversely with the user's scroll speed.
                    // `--inout-dur` drives the fold-in clip; the same
                    // mapping sets the slide spring's stiffness.
                    let dur_ms = inout_dur_ms(scroll_vel);
                    let _ = el.style().set_property("--inout-dur", &format!("{dur_ms:.0}ms"));
                    let _ = c2.add_1("folding");
                    let _ = c2.add_1("fold-anim");
                    // Phase C: the slide up into the stack top is a
                    // Pin spring, not a CSS keyframe. The card starts
                    // at its flow position (dy 0); the gluing pins its
                    // r=0 target every frame and the spring settles
                    // the residual — an interrupted scroll retargets
                    // the glide instead of restarting an animation.
                    if !st.reduced_motion {
                        spring_adopt(st, el, true, 0.0, 0, now_t, spring_omega(scroll_vel));
                    } else {
                        spring_drop(st, el);
                    }
                    st.deal_dbg_hist.push(format!(
                        "dock-pending nat={:.0} slot={:.0} s_top={:.0} dur={:.0}",
                        nat, slot_top, s_top, dur_ms
                    ));
                    if st.deal_dbg_hist.len() > 12 {
                        st.deal_dbg_hist.remove(0);
                    }
                    // Phase B settle-driven commit watcher.
                    st.fold_watch.push((el.clone(), now_t));
                    let el2 = el.clone();
                    let to = gloo_timers::callback::Timeout::new(80, move || {
                        with_pile(|cell| {
                            let s = cell.borrow();
                            if let Some(s) = s.as_ref() {
                                let mut st = s.borrow_mut();
                                watch_fold_commit(&mut st, &el2, dur_ms);
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
    // while scrolling up. The 12 px band (92 down / 104 up) is the
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
                "delta={:.1} changed={} slot={:.0} thr={:.0} deal={} drain=false s_top={:.0} dur={:.0}",
                delta, changed, slot_newest, threshold, deal, s_top,
                inout_dur_ms(scroll_vel)
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
            // peel-out slide starts at that pin and ends at the
            // (corrected) flow slot, so it reads as the deck's front
            // face sliding down out of the pile into the queue.
            // (Inverted stack: layer `from_end` sits ABOVE the newest
            // face, at PILE_TOP - from_end*PEEK.) Phase C: the slide
            // is a Flow spring (--deck-dy decays deal_dy → 0), not
            // the old CSS unfold-slide keyframe; the unfold-in clip
            // (velocity-matched via --inout-dur) runs in parallel.
            let deal_dy = if from_end < DECK_SHOW {
                PILE_TOP - from_end as f64 * DECK_PEEK - (slot_top - corr)
            } else {
                0.0
            };
            // Velocity-matched in/out (mirror of the fold side): the
            // clip duration scales inversely with the user's scroll
            // speed; the same mapping sets the slide spring's stiffness.
            let dur_ms = inout_dur_ms(scroll_vel);
            let _ = el.style().set_property("--inout-dur", &format!("{dur_ms:.0}ms"));

            // Drain batches stagger via the spring delay (the old CSS
            // animation-delay) and, for the clip, via animation-delay.
            let delay_ms = (batch * 50).min(250);
            if delay_ms > 0 {
                let _ = el.style().set_property("animation-delay", &format!("{delay_ms}ms"));
            }
            if !st.reduced_motion {
                spring_adopt(st, el, false, deal_dy, delay_ms as u32, now_t, spring_omega(scroll_vel));
                let _ = el.style().set_property("--deck-dy", &format!("{deal_dy:.0}px"));
            }
            let _ = el.offset_width(); // reflow
            let _ = el.class_list().add_1("unfold-anim");
            // z 51 rides on the .unfold-anim class (above the deck's
            // 50) for the whole slide-out, so the card reads as the
            // top card sliding down in FRONT of the pile, revealing
            // the promoted face behind — the exact reverse of the
            // dock's slide-up. When the spring settles, its settle
            // path sheds the class + inline state, and the card falls
            // back to queue layering — invisible, and self-healing if
            // a re-dock happens in the meantime.
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
        // Fresh borrow: the top-of-function `tr` was block-scoped so
        // the spring bookkeeping above could mutate `st`.
        let tr = st.transcript.as_ref().unwrap();
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
    // Transcript top (for spring start-point measurements).
    let gl_top = st.transcript
        .as_ref()
        .map(|t| t.get_bounding_client_rect().top())
        .unwrap_or(0.0);
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
        let base_dy = glued_y - slot_top + scroll_shift;
        let prev_r = prev_deck
            .iter()
            .find(|(e, _)| same_el(e, el))
            .map(|(_, r)| *r);
        // Phase C: an in-flight slide spring (a cascade commit, a
        // layer-shift) owns this row's transform: integrate it
        // against this frame's pin target and write the live value.
        // The target recomputes every frame, so a mid-gesture scroll
        // retargets the glide instead of restarting it.
        if let Some(i) = spring_idx(st, el) {
            let mut sp = st.springs[i].clone();
            let settled = spring_step(&mut sp, base_dy, now_t);
            let w = if settled { base_dy.round() } else { sp.x };
            if settled {
                st.springs.remove(i);
            } else {
                st.springs[i] = sp;
            }
            let _ = el.style().set_property("--deck-dy", &format!("{w:.1}px"));
        } else if prev_r.is_some_and(|pr| pr != r as i32) && !st.reduced_motion {
            // This row's slot changed since the last frame (a card
            // dealt out, a new face committed): glide it from where
            // it currently renders (stale transform included) into
            // the new slot instead of snapping — a spring, not a
            // keyframe.
            // Start the glide from the row's CURRENT transform (the
            // same `old_dy + shift` convention repin_layer uses): the
            // transform is scroll-invariant, so re-anchoring off the
            // rendered position would bake the scroll offset in.
            let old_dy = el
                .style()
                .get_property_value("--deck-dy")
                .ok()
                .and_then(|v| v.trim_end_matches("px").trim().parse::<f64>().ok())
                .unwrap_or(0.0);
            let x0 = old_dy + scroll_shift;
            spring_adopt(st, el, true, x0, 0, now_t, spring_omega(scroll_vel));
            let _ = el.style().set_property("--deck-dy", &format!("{x0:.1}px"));
        } else {
            // First pin (or settled spring): the static pin value.
            let _ = el.style().set_property("--deck-dy", &format!("{:.1}px", base_dy.round()));
        }
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
    // fold window; its commit swaps it into the compact deck-layer
    // set on the next frame. Phase C: the dock slide into the stack
    // top is a Pin spring (adopted in the dock-pending branch),
    // integrated here against the live pin target.
    for (el, slot_top) in &pending_slots {
        let base_dy = PILE_TOP - slot_top + scroll_shift;
        if let Some(i) = spring_idx(st, el) {
            let mut sp = st.springs[i].clone();
            let settled = spring_step(&mut sp, base_dy, now_t);
            let w = if settled { base_dy.round() } else { sp.x };
            if settled {
                st.springs.remove(i);
            } else {
                st.springs[i] = sp;
            }
            let _ = el.style().set_property("--deck-dy", &format!("{w:.1}px"));
        } else {
            let _ = el.style().set_property("--deck-dy", &format!("{:.1}px", base_dy.round()));
        }
        let _ = el.style().set_property("z-index", "51");
        let _ = el.class_list().add_1("deck-layer");
        new_deck.push((el.clone(), 0));
    }
    // Clear pinning from cards that left the deck. A compact row
    // pushed off the visible 4-layer stack glides back to its flow
    // position and fades its edge out (leave_deck's Flow spring)
    // instead of vanishing mid-cascade; anything else clears.
    // A card with a live flow spring (a dealt / leaving edge) is
    // left alone — its settle path or leave timer releases it.
    for (el, _r) in &prev_deck {
        if !new_deck.iter().any(|(e, _)| same_el(e, el)) {
            if compact.iter().any(|(e, _)| same_el(e, el)) {
                leave_deck(st, el, now_t);
            } else if spring_idx(st, el).is_none() {
                clear_deck(el);
            }
        }
    }
    st.deck_layers = new_deck;
    // Phase C: integrate the flow-targeted springs (dealt cards,
    // leaving edges, cancelled folds) and keep the rAF loop alive
    // while any spring is still settling, even at rest (no scroll
    // event to drive it).
    step_flow_springs(st, now_t);
    if !st.springs.is_empty() {
        schedule_step_locked(st);
    }
}

/// Push a deck layer off the stack (Phase C): instead of holding its
/// vacated slot, the edge glides back to its natural flow position
/// (a Flow spring drives --deck-dy down to 0) while `deck-leave`
/// fades it out, tucking behind the cascading promoted layers. A
/// leave watcher (watch_deck_leave) clears the pin + classes once
/// the edge has settled out of the deck; a re-promotion mid-cascade
/// sheds the fade so the card reappears behind the cascade instead.
/// Idempotent: an already-fading layer is owned by its watcher.
fn leave_deck(st: &mut PileState, le: &HtmlElement, now_ms: f64) {
    let cls = le.class_list();
    if cls.contains("deck-leave") {
        return; // a leave watcher already owns this layer
    }
    let s = le.style();
    let old_dy = s
        .get_property_value("--deck-dy")
        .ok()
        .and_then(|v| v.trim_end_matches("px").trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    // Phase C: instead of holding the vacated slot (the old deck-shift
    // hold-slide), the edge glides back DOWN to its natural flow
    // position — where it really is in the compact stack — while it
    // fades, tucking itself behind the cascading promoted layers.
    // A Flow spring owns the transform; the gluing stale-cleanup
    // leaves it alone (it's no longer in the pin set), and the settle
    // path (step_flow_springs) releases the transform at flow.
    if !st.reduced_motion {
        spring_adopt(st, le, false, old_dy, 0, now_ms, spring_omega(0.0));
        let _ = s.set_property("--deck-dy", &format!("{old_dy:.1}px"));
    } else {
        spring_drop(st, le);
        let _ = s.remove_property("--deck-dy");
    }
    let _ = cls.add_1("deck-leave");
    // Watcher: after the fade window, clear the pin + class once the
    // edge has settled OUT of the deck; a re-promotion mid-cascade
    // instead sheds the fade so the card reappears behind the
    // cascade.
    let el2 = le.clone();
    let to = gloo_timers::callback::Timeout::new(450, move || {
        with_pile(|cell| {
            let s = cell.borrow();
            if let Some(s) = s.as_ref() {
                let mut st = s.borrow_mut();
                watch_deck_leave(&mut st, &el2);
            }
        });
    });
    LEAKED.with(|l| l.borrow_mut().push(Box::new(to)));
}

/// Re-check (every 250 ms) whether a leaving edge may be cleared:
/// out of the deck → clear the pin + fade class; back in the deck
/// (re-promoted by a deal during the fade) → shed the fade so the
/// card is visible again and the gluing owns its pin; detached →
/// retire.
fn watch_deck_leave(st: &mut PileState, el: &HtmlElement) {
    if st.deck_layers.iter().any(|(e, _)| same_el(e, el)) {
        // Re-promoted: visible again, gluing owns the pin from here.
        let _ = el.class_list().remove_1("deck-leave");
        spring_drop(st, el);
        return;
    }
    let node: web_sys::Node = el.clone().unchecked_into();
    if !node.is_connected() {
        spring_drop(st, el);
        return; // detached: nothing left to clean
    }
    clear_deck(el);
    spring_drop(st, el);
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
    let _ = cls.remove_1("deck-leave");
    let _ = cls.remove_1("deck-gliding");
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
