//! M0 webview spike instrumentation: batch-to-paint latency and scroll frame times.
//! Active only when the shell runs in spike mode; it scrolls the list itself and reports.
use std::cell::RefCell;

use leptos::html::Div;
use leptos::prelude::*;
use serde_json::json;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::ipc;

thread_local! {
    static BATCHES: RefCell<Vec<(f64, bool)>> = const { RefCell::new(Vec::new()) };
    static SCROLLER: RefCell<Option<NodeRef<Div>>> = const { RefCell::new(None) };
}

pub fn record_batch(ms: f64, init: bool) {
    BATCHES.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() < 100_000 {
            b.push((ms, init));
        }
    });
}

/// A self-rescheduling animation-frame callback.
type FrameLoop = std::rc::Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

pub fn register_scroller(r: NodeRef<Div>) {
    SCROLLER.with(|s| *s.borrow_mut() = Some(r));
}

fn scroller() -> Option<web_sys::HtmlDivElement> {
    SCROLLER.with(|s| s.borrow().as_ref().and_then(|r| r.get_untracked()))
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn summary(mut v: Vec<f64>) -> serde_json::Value {
    let n = v.len();
    let max = v.iter().cloned().fold(0.0, f64::max);
    json!({"count": n, "p50": percentile(&mut v, 0.5), "p95": percentile(&mut v, 0.95),
           "p99": percentile(&mut v, 0.99), "max": max})
}

/// Pixels scrolled per animation frame, and frames per direction (about 20 s at 60 Hz).
const SCROLL_STEP_PX: i32 = 400;
const SCROLL_FRAMES: u32 = 1200;

/// Scrolls up through older history and back down, recording frame intervals,
/// then reports everything to the shell.
pub fn run(seconds_live: u32) {
    let frames: std::rc::Rc<RefCell<Vec<f64>>> = Default::default();
    let state = std::rc::Rc::new(RefCell::new((0u32, ipc::now_ms(), 0i32, 0u32)));
    // (phase, last frame time, last scroll top, stuck frames)
    let f = frames.clone();
    let cb: FrameLoop = Default::default();
    let cb2 = cb.clone();
    let start = ipc::now_ms();
    *cb.borrow_mut() = Some(Closure::new(move || {
        let now = ipc::now_ms();
        let mut st = state.borrow_mut();
        let dt = now - st.1;
        st.1 = now;
        let live_done = now - start > seconds_live as f64 * 1000.0;
        if st.0 > 0 {
            f.borrow_mut().push(dt);
        }
        if let Some(el) = scroller() {
            match st.0 {
                0 if live_done => {
                    BATCHES.with(|b| b.borrow_mut().clear());
                    st.0 = 1;
                }
                0 => {}
                1 => {
                    el.set_scroll_top(el.scroll_top() - SCROLL_STEP_PX);
                    st.3 += 1;
                    if st.3 > SCROLL_FRAMES {
                        st.0 = 2;
                        st.3 = 0;
                    }
                }
                2 => {
                    el.set_scroll_top(el.scroll_top() + SCROLL_STEP_PX);
                    st.3 += 1;
                    if st.3 > SCROLL_FRAMES {
                        st.0 = 3;
                    }
                }
                _ => {}
            }
        }
        if st.0 == 3 {
            let frames = f.borrow().clone();
            let long = frames.iter().filter(|d| **d > 50.0).count();
            let (live, scroll_inits): (Vec<_>, Vec<_>) = BATCHES
                .with(|b| b.borrow().clone())
                .into_iter()
                .partition(|(_, init)| !init);
            let report = json!({
                "scroll_frames": summary(frames.clone()),
                "scroll_long_frames_over_50ms": long,
                "scroll_window_moves": scroll_inits.len(),
                "scroll_window_move_paint_ms": summary(scroll_inits.into_iter().map(|x| x.0).collect()),
                "scroll_deltas_paint_ms": summary(live.into_iter().map(|x| x.0).collect()),
            });
            ipc::command("spike_report", json!({"phase": "scroll", "report": report}));
            return;
        }
        if let (Some(w), Some(c)) = (web_sys::window(), cb2.borrow().as_ref()) {
            let _ = w.request_animation_frame(c.as_ref().unchecked_ref());
        }
    }));
    // Report the live-burst phase separately, before scrolling clears it.
    let live_ms = seconds_live as i32 * 1000;
    let report_live = Closure::once_into_js(move || {
        let batches: Vec<f64> =
            BATCHES.with(|b| b.borrow().iter().filter(|(_, i)| !i).map(|x| x.0).collect());
        ipc::command(
            "spike_report",
            json!({"phase": "live", "report": {"batch_to_paint_ms": summary(batches)}}),
        );
    });
    if let Some(w) = web_sys::window() {
        let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
            report_live.unchecked_ref(),
            live_ms - 50,
        );
        if let Some(c) = cb.borrow().as_ref() {
            let _ = w.request_animation_frame(c.as_ref().unchecked_ref());
        }
    }
    std::mem::forget(cb);
}
