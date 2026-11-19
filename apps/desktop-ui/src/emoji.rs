//! Emoji search, and the picker: a search field over a grid, with the account's recent emoji
//! on top. Names and shortcodes come from the data-only `emojis` crate.
use leptos::html::{Div, Input};
use leptos::prelude::*;
use serde_json::json;
use uuid::Uuid;
use wasm_bindgen::JsCast;

use crate::ipc;

const GRID: [&str; 48] = [
    "👍", "👎", "😀", "😂", "🙂", "😉", "😍", "🤔", "😮", "😢", "😡", "🙏", "👏", "🙌", "💪", "👀",
    "🎉", "🔥", "✅", "❌", "⚠️", "❤️", "💯", "🚀", "⭐", "💡", "📌", "📎", "🐛", "🛠️", "📅", "⏰",
    "☕", "🍕", "🎂", "🌱", "☀️", "🌧️", "🌙", "👋", "🤝", "✨", "😅", "😴", "🤯", "🥳", "🙈", "➕",
];

/// Cells per grid row; the arrow keys move by this much up and down.
const COLUMNS: usize = 8;

/// An emoji found for a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub emoji: &'static str,
    /// `:tada:`, or the name when the emoji has no shortcode.
    pub label: String,
    /// The emoji's name, when the name matched rather than a shortcode.
    pub hint: Option<&'static str>,
}

fn label(e: &emojis::Emoji) -> String {
    e.shortcode()
        .map_or_else(|| e.name().to_string(), |c| format!(":{c}:"))
}

/// The emoji's name, for screen readers and tooltips.
pub fn name(emoji: &str) -> String {
    emojis::get(emoji).map_or_else(|| emoji.to_string(), |e| e.name().to_string())
}

/// Emoji for `q`, best first: a shortcode that is `q`, one that starts with it, a word of the
/// name that starts with it, then a shortcode or name that contains it.
pub fn search(q: &str, limit: usize) -> Vec<Found> {
    let q = q.trim().trim_matches(':').to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let code_q = q.replace(' ', "_");
    let mut hits: Vec<(u8, usize, &'static emojis::Emoji)> = vec![];
    for (i, e) in emojis::iter().enumerate() {
        let name = e.name().to_lowercase();
        let rank = if e.shortcodes().any(|c| c == code_q) {
            0
        } else if e.shortcodes().any(|c| c.starts_with(&code_q)) {
            1
        } else if name
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w.starts_with(&q))
            || name.starts_with(&q)
        {
            2
        } else if e.shortcodes().any(|c| c.contains(&code_q)) || name.contains(&q) {
            3
        } else {
            continue;
        };
        hits.push((rank, i, e));
    }
    // Shorter shortcodes first within a rank: `:ta` offers 🎉 before 🍊.
    hits.sort_by_key(|(r, i, e)| (*r, e.shortcode().map_or(usize::MAX, str::len), *i));
    hits.into_iter()
        .take(limit)
        .map(|(rank, _, e)| Found {
            emoji: e.as_str(),
            label: label(e),
            hint: (rank >= 2 && !e.shortcodes().any(|c| c.contains(&code_q))).then(|| e.name()),
        })
        .collect()
}

/// Where focus goes from cell `i` of `n` on `key`; `None` leaves the grid for the search field.
pub fn grid_move(key: &str, i: usize, n: usize) -> Option<Option<usize>> {
    let last = n.checked_sub(1)?;
    Some(match key {
        "ArrowRight" => Some((i + 1).min(last)),
        "ArrowLeft" => Some(i.saturating_sub(1)),
        "ArrowDown" => Some((i + COLUMNS).min(last)),
        "ArrowUp" if i < COLUMNS => None,
        "ArrowUp" => Some(i - COLUMNS),
        "Home" => Some(0),
        "End" => Some(last),
        _ => return None,
    })
}

fn cells(panel: NodeRef<Div>) -> Vec<web_sys::HtmlElement> {
    let Some(list) = panel
        .get_untracked()
        .and_then(|p| p.query_selector_all(".emo").ok())
    else {
        return vec![];
    };
    (0..list.length())
        .filter_map(|i| list.item(i)?.dyn_into::<web_sys::HtmlElement>().ok())
        .collect()
}

/// The picker floats over a transparent backdrop that closes it on any outside click. Focus
/// starts in the search field and goes back to where it was when the picker closes.
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
        let top = if y + 4.0 + 360.0 > h {
            (y - 34.0 - 360.0).max(8.0)
        } else {
            y + 4.0
        };
        format!(
            "position: fixed; left: {}px; top: {top}px; right: auto; bottom: auto",
            (x - 296.0).clamp(8.0, w - 304.0)
        )
    });
    let recent = RwSignal::new(Vec::<String>::new());
    let query = RwSignal::new(String::new());
    let panel: NodeRef<Div> = NodeRef::new();
    let field: NodeRef<Input> = NodeRef::new();
    let opener = StoredValue::new_local(
        document()
            .active_element()
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()),
    );
    let give_back = move || {
        if let Some(Some(el)) = opener.try_get_value() {
            let _ = el.focus();
        }
    };
    let close = move || {
        give_back();
        on_close.run(());
    };
    let pick = Callback::new(move |e: String| {
        give_back();
        on_pick.run(e);
    });
    Effect::new(move |_| {
        if let Some(f) = field.get() {
            let _ = f.focus();
        }
    });
    leptos::task::spawn_local(async move {
        if let Ok(r) =
            ipc::invoke::<Vec<String>>("recent_emoji", json!({"accountId": account_id})).await
        {
            recent.set(r);
        }
    });
    let results = Memo::new(move |_| query.with(|q| search(q, 64)));
    let cell = move |e: String, title: String| {
        let pick_this = e.clone();
        let spoken = name(&e);
        view! {
            <button type="button" class="emo" tabindex="-1" aria-label=spoken title=title
                on:click=move |ev| { ev.stop_propagation(); pick.run(pick_this.clone()); }>{e}</button>
        }
    };
    let titled = move |e: &str| emojis::get(e).map_or_else(|| e.to_string(), label);
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        if key == "Escape" {
            ev.prevent_default();
            ev.stop_propagation();
            close();
            return;
        }
        let in_field = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            .is_some();
        let all = cells(panel);
        if in_field {
            match key.as_str() {
                "ArrowDown" => {
                    if let Some(c) = all.first() {
                        ev.prevent_default();
                        let _ = c.focus();
                    }
                }
                "Enter" => {
                    ev.prevent_default();
                    if let Some(c) = all.first() {
                        c.click();
                    }
                }
                _ => {}
            }
            return;
        }
        let Some(i) = ev.target().and_then(|t| {
            all.iter()
                .position(|c| AsRef::<web_sys::EventTarget>::as_ref(c) == &t)
        }) else {
            return;
        };
        if let Some(to) = grid_move(&key, i, all.len()) {
            ev.prevent_default();
            match to {
                Some(j) => {
                    let _ = all[j].focus();
                }
                None => {
                    if let Some(f) = field.get_untracked() {
                        let _ = f.focus();
                    }
                }
            }
        } else if key.chars().count() == 1 && !ev.meta_key() && !ev.ctrl_key() && key != " " {
            // Typing in the grid goes on in the search field.
            if let Some(f) = field.get_untracked() {
                let _ = f.focus();
            }
        }
    };
    view! {
        <div class="backdrop" on:click=move |_| close()></div>
        <div class=format!("picker {placement}") style=style role="dialog" aria-label="Pick an emoji" node_ref=panel
             on:keydown=on_key>
            <input type="search" class="psearch" node_ref=field placeholder="Search emoji" aria-label="Search emoji"
                autocomplete="off" spellcheck="false"
                prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
            <div class="pscroll">
                <Show when=move || query.with(|q| q.trim().is_empty())
                    fallback=move || view! {
                        <div class="ptitle">"Results"</div>
                        <Show when=move || results.with(|r| !r.is_empty())
                            fallback=|| view! { <p class="pnone">"No emoji found"</p> }>
                            <div class="pgrid" role="group" aria-label="Results">
                                {move || results.get().into_iter().map(|f| cell(f.emoji.to_string(), f.label)).collect_view()}
                            </div>
                        </Show>
                    }>
                    <Show when=move || recent.with(|r| !r.is_empty())>
                        <div class="ptitle">"Recent"</div>
                        <div class="pgrid" role="group" aria-label="Recent">
                            {move || recent.get().into_iter().take(COLUMNS).map(|e| { let t = titled(&e); cell(e, t) }).collect_view()}
                        </div>
                    </Show>
                    <div class="ptitle">"Emoji"</div>
                    <div class="pgrid" role="group" aria-label="Emoji">
                        {GRID.iter().map(|e| cell(e.to_string(), titled(e))).collect_view()}
                    </div>
                </Show>
            </div>
            <p class="visually-hidden" aria-live="polite">{move || {
                let q = query.get();
                if q.trim().is_empty() { String::new() } else {
                    match results.with(Vec::len) { 0 => "No emoji found".to_string(), 1 => "1 emoji".to_string(), n => format!("{n} emoji") }
                }
            }}</p>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji(q: &str) -> Vec<&'static str> {
        search(q, 5).into_iter().map(|f| f.emoji).collect()
    }

    #[test]
    fn search_ranks_shortcodes_then_names() {
        assert_eq!(emoji("tada")[0], "🎉");
        assert_eq!(emoji(":tada:")[0], "🎉");
        let ta = search("ta", 6);
        assert!(ta.iter().any(|f| f.emoji == "🎉"));
        assert!(ta.iter().any(|f| f.emoji == "🌮"));
        // A word of the name: "party popper" for 🎉.
        let party = search("popper", 5);
        assert_eq!(party[0].emoji, "🎉");
        assert_eq!(party[0].hint, Some("party popper"));
        assert_eq!(search("+1", 1)[0].emoji, "👍");
        assert_eq!(search("thumbs up", 1)[0].emoji, "👍");
        assert!(search("", 5).is_empty());
        assert!(search("zzzzqqq", 5).is_empty());
        assert_eq!(search("smile", 3).len(), 3);
        assert_eq!(search("tada", 1)[0].label, ":tada:");
    }

    #[test]
    fn grid_keys() {
        assert_eq!(grid_move("ArrowRight", 0, 20), Some(Some(1)));
        assert_eq!(grid_move("ArrowRight", 19, 20), Some(Some(19)));
        assert_eq!(grid_move("ArrowLeft", 0, 20), Some(Some(0)));
        assert_eq!(grid_move("ArrowDown", 3, 20), Some(Some(11)));
        assert_eq!(grid_move("ArrowDown", 15, 20), Some(Some(19)));
        assert_eq!(grid_move("ArrowUp", 11, 20), Some(Some(3)));
        assert_eq!(
            grid_move("ArrowUp", 3, 20),
            Some(None),
            "back to the search field"
        );
        assert_eq!(grid_move("End", 0, 20), Some(Some(19)));
        assert_eq!(grid_move("a", 0, 20), None);
        assert_eq!(grid_move("ArrowRight", 0, 0), None);
    }
}
