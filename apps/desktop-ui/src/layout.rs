//! Pane sizes: the sidebar and thread widths, their drag handles, and when the thread overlays.
use leptos::prelude::*;
use proto::view::{SIDEBAR_WIDTH, THREAD_WIDTH, UiPrefs};
use serde_json::json;
use wasm_bindgen::JsCast;

use crate::ipc;
use crate::state::ctx;

/// The rail's fixed width, beside the sidebar.
pub const RAIL: u32 = 64;
/// The message column never gets narrower than this; below it the thread overlays.
pub const MAIN_MIN: u32 = 480;

/// The sidebar width to draw: the chosen one, narrowed so the message column keeps its minimum.
pub fn sidebar_width(prefs: UiPrefs, window: f64) -> u32 {
    let room = (window as i64 - (RAIL + MAIN_MIN) as i64).max(0) as u32;
    prefs.sidebar().min(room).max(SIDEBAR_WIDTH.0)
}

/// Whether an open thread would squeeze the message column below its minimum.
pub fn thread_overlays(prefs: UiPrefs, window: f64) -> bool {
    let used = RAIL + sidebar_width(prefs, window) + prefs.thread();
    (window as i64) - (used as i64) < MAIN_MIN as i64
}

pub fn save(prefs: UiPrefs) {
    ipc::command("set_ui_prefs", json!({ "prefs": prefs }));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Sidebar,
    Thread,
}

impl Pane {
    fn limits(self) -> (u32, u32, u32) {
        match self {
            Pane::Sidebar => SIDEBAR_WIDTH,
            Pane::Thread => THREAD_WIDTH,
        }
    }

    fn width(self, p: &UiPrefs) -> u32 {
        match self {
            Pane::Sidebar => p.sidebar(),
            Pane::Thread => p.thread(),
        }
    }

    fn set(self, p: &mut UiPrefs, w: Option<u32>) {
        match self {
            Pane::Sidebar => p.sidebar_width = w,
            Pane::Thread => p.thread_width = w,
        }
    }

    /// The thread sits on the right, so dragging its edge left makes it wider.
    fn grow(self) -> i32 {
        match self {
            Pane::Sidebar => 1,
            Pane::Thread => -1,
        }
    }
}

fn body_style(cursor: &str) {
    if let Some(b) = document().body() {
        let s = b.style();
        let _ = s.set_property("cursor", cursor);
        let _ = s.set_property("user-select", if cursor.is_empty() { "" } else { "none" });
    }
}

/// A vertical drag handle on a pane's edge. Drag, arrow keys, Home/End, and double-click or
/// Return to reset. Window-level move listeners, so a fast drag never loses the pointer.
#[component]
pub fn Grip(pane: Pane) -> impl IntoView {
    let c = ctx();
    let (min, default, max) = pane.limits();
    let drag = StoredValue::new(None::<(f64, u32)>);
    let dragging = RwSignal::new(false);
    let width = move || pane.width(&c.prefs.get());
    let set = move |w: Option<u32>, persist: bool| {
        c.prefs
            .update(|p| pane.set(p, w.map(|w| w.clamp(min, max))));
        if persist {
            save(c.prefs.get_untracked());
        }
    };
    let _move = window_event_listener(leptos::ev::pointermove, move |ev| {
        if let Some((x0, w0)) = drag.get_value() {
            let dx = (ev.client_x() as f64 - x0) as i32 * pane.grow();
            set(Some((w0 as i32 + dx).max(0) as u32), false);
        }
    });
    let _up = window_event_listener(leptos::ev::pointerup, move |_| {
        if drag.get_value().is_some() {
            drag.set_value(None);
            dragging.set(false);
            body_style("");
            save(c.prefs.get_untracked());
        }
    });
    let label = match pane {
        Pane::Sidebar => "Resize sidebar",
        Pane::Thread => "Resize thread",
    };
    view! {
        <div class="grip" class:thr=pane == Pane::Thread class:hot=move || dragging.get()
            role="separator" aria-orientation="vertical" aria-label=label tabindex="0"
            aria-valuemin=min aria-valuemax=max aria-valuenow=width
            aria-valuetext=move || format!("{} pixels", width())
            title="Drag to resize · double-click to reset"
            on:pointerdown=move |ev: web_sys::PointerEvent| {
                if ev.button() != 0 {
                    return;
                }
                ev.prevent_default();
                if let Some(t) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok()) {
                    let _ = t.focus();
                }
                drag.set_value(Some((ev.client_x() as f64, width())));
                dragging.set(true);
                body_style("col-resize");
            }
            on:dblclick=move |_| set(None, true)
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                let step = if ev.shift_key() { 64 } else { 16 };
                let w = width() as i32;
                let next = match ev.key().as_str() {
                    "ArrowRight" => Some(w + step * pane.grow()),
                    "ArrowLeft" => Some(w - step * pane.grow()),
                    "Home" => Some(min as i32),
                    "End" => Some(max as i32),
                    "Enter" => Some(default as i32),
                    _ => None,
                };
                if let Some(n) = next {
                    ev.prevent_default();
                    set(Some(n.max(0) as u32), true);
                }
            }>
            <div class="ln"></div>
            {move || dragging.get().then(|| view! { <div class="tip">{format!("{} px · double-click to reset", width())}</div> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_column_keeps_its_minimum() {
        let wide = UiPrefs {
            sidebar_width: Some(420),
            thread_width: None,
        };
        // 900 px window: 900 - 64 - 480 leaves 356 for the sidebar.
        assert_eq!(sidebar_width(wide, 900.0), 356);
        assert_eq!(sidebar_width(wide, 1600.0), 420);
        // Never below the sidebar's own minimum.
        assert_eq!(sidebar_width(wide, 600.0), 208);
        assert!(thread_overlays(UiPrefs::default(), 1024.0));
        assert!(!thread_overlays(UiPrefs::default(), 1280.0));
    }
}
