//! A compact emoji grid with the account's recent emoji on top.
use leptos::prelude::*;
use serde_json::json;
use uuid::Uuid;

use crate::ipc;

const GRID: [&str; 48] = [
    "👍", "👎", "😀", "😂", "🙂", "😉", "😍", "🤔", "😮", "😢", "😡", "🙏", "👏", "🙌", "💪", "👀",
    "🎉", "🔥", "✅", "❌", "⚠️", "❤️", "💯", "🚀", "⭐", "💡", "📌", "📎", "🐛", "🛠️", "📅", "⏰",
    "☕", "🍕", "🎂", "🌱", "☀️", "🌧️", "🌙", "👋", "🤝", "✨", "😅", "😴", "🤯", "🥳", "🙈", "➕",
];

/// The picker floats over a transparent backdrop that closes it on any outside click.
#[component]
pub fn EmojiPicker(
    account_id: Uuid,
    on_pick: Callback<String>,
    on_close: Callback<()>,
    /// Where the panel sits: `"up"` (above the trigger) or `"down"`.
    #[prop(default = "up")]
    placement: &'static str,
    /// A viewport point to float at, so a scrolling list never clips the panel.
    #[prop(optional)]
    at: Option<(f64, f64)>,
) -> impl IntoView {
    let style = at.map(|(x, y)| {
        let (w, h) = web_sys::window()
            .map(|w| {
                let n = |v: Result<wasm_bindgen::JsValue, _>| {
                    v.ok().and_then(|v| v.as_f64()).unwrap_or(800.0)
                };
                (n(w.inner_width()), n(w.inner_height()))
            })
            .unwrap_or((1200.0, 800.0));
        let top = if y + 4.0 + 330.0 > h {
            (y - 34.0 - 330.0).max(8.0)
        } else {
            y + 4.0
        };
        format!(
            "position: fixed; left: {}px; top: {top}px; right: auto; bottom: auto",
            (x - 296.0).clamp(8.0, w - 304.0)
        )
    });
    let recent = RwSignal::new(Vec::<String>::new());
    leptos::task::spawn_local(async move {
        if let Ok(r) =
            ipc::invoke::<Vec<String>>("recent_emoji", json!({"accountId": account_id})).await
        {
            recent.set(r);
        }
    });
    let cell = move |e: String| {
        let label = e.clone();
        let pick = e.clone();
        view! {
            <button type="button" class="emo" role="gridcell" aria-label=label
                on:click=move |ev| { ev.stop_propagation(); on_pick.run(pick.clone()); }>{e}</button>
        }
    };
    view! {
        <div class="backdrop" on:click=move |_| on_close.run(())></div>
        <div class=format!("picker {placement}") style=style role="dialog" aria-label="Pick an emoji"
             on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Escape" { ev.prevent_default(); on_close.run(()) }>
            <Show when=move || recent.with(|r| !r.is_empty())>
                <div class="ptitle">"Recent"</div>
                <div class="pgrid" role="grid">
                    {move || recent.get().into_iter().take(8).map(cell).collect_view()}
                </div>
            </Show>
            <div class="ptitle">"Emoji"</div>
            <div class="pgrid" role="grid">
                {GRID.iter().map(|e| cell(e.to_string())).collect_view()}
            </div>
        </div>
    }
}
