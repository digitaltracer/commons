//! Workspace search: a box in the conversation header and a results panel under it.
use leptos::prelude::*;
use proto::api::SearchHitKind;
use proto::view::SearchHitView;
use serde_json::json;
use uuid::Uuid;

use crate::app::Selection;
use crate::ipc;

#[derive(Clone, Debug, PartialEq)]
pub enum Results {
    Idle,
    Searching,
    Done(String, Vec<SearchHitView>),
    Failed(String),
}

#[component]
pub fn SearchBox(
    account_id: Uuid,
    workspace_id: Uuid,
    results: RwSignal<Results>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let run = move || {
        let q = text.get_untracked().trim().to_string();
        if q.is_empty() {
            results.set(Results::Idle);
            return;
        }
        results.set(Results::Searching);
        leptos::task::spawn_local(async move {
            let r: Result<Vec<SearchHitView>, String> = ipc::invoke(
                "search",
                json!({"accountId": account_id, "workspaceId": workspace_id, "query": q}),
            )
            .await;
            results.set(match r {
                Ok(hits) => Results::Done(q, hits),
                Err(e) => Results::Failed(e),
            });
        });
    };
    view! {
        <form class="search" role="search" on:submit=move |ev| { ev.prevent_default(); run(); }>
            <label class="visually-hidden" for=format!("search-{workspace_id}")>"Search this workspace"</label>
            <input id=format!("search-{workspace_id}") type="search" placeholder="Search"
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Escape" {
                        text.set(String::new());
                        results.set(Results::Idle);
                    }
                } />
        </form>
    }
}

fn when(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    format!(
        "{}-{:02}-{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date()
    )
}

#[component]
pub fn SearchResults(
    account_id: Uuid,
    results: RwSignal<Results>,
    selected: RwSignal<Option<Selection>>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let open = move |hit: SearchHitView| {
        let sel = Selection {
            account_id,
            conversation_id: hit.conversation_id,
        };
        if hit.joined {
            results.set(Results::Idle);
            selected.set(Some(sel));
            return;
        }
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": account_id, "conversationId": hit.conversation_id});
            match ipc::invoke::<serde_json::Value>("join_channel", args).await {
                Ok(_) => {
                    results.set(Results::Idle);
                    selected.set(Some(sel));
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let body = move || match results.get() {
        Results::Idle => ().into_any(),
        Results::Searching => view! { <p class="meta">"Searching…"</p> }.into_any(),
        Results::Failed(e) => {
            view! { <p class="error" role="alert">{format!("Search failed: {e}")}</p> }.into_any()
        }
        Results::Done(q, hits) if hits.is_empty() => {
            view! { <p class="meta">{format!("Nothing found for “{q}”.")}</p> }.into_any()
        }
        Results::Done(q, hits) => {
            let count = format!("{} results for “{q}”", hits.len());
            view! {
                <p class="meta">{count}
                    <button class="linkish" on:click=move |_| results.set(Results::Idle)>"Close"</button></p>
                <ul>
                    {hits.into_iter().map(|hit| {
                        let place = match (&hit.kind, &hit.file_name) {
                            (SearchHitKind::Message, _) | (_, None) => hit.conversation_title.clone(),
                            (_, Some(f)) => format!("{} · file {f}", hit.conversation_title),
                        };
                        let action = if hit.joined { "Open" } else { "Join and open" };
                        let label = format!("{action} {place}");
                        let snippet = hit.snippet.iter().map(|p| {
                            let t = p.text.clone();
                            if p.hit { view! { <mark>{t}</mark> }.into_any() } else { t.into_any() }
                        }).collect_view();
                        let target = hit.clone();
                        view! {
                            <li>
                                <button class="hit" aria-label=label on:click=move |_| open(target.clone())>
                                    <span class="where">{place}" · "{when(hit.created_at_ms)}
                                        {(!hit.joined).then_some(" · not joined")}</span>
                                    <span class="snippet">{snippet}</span>
                                </button>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }.into_any()
        }
    };
    view! {
        <Show when=move || results.with(|r| *r != Results::Idle)>
            <section class="search-results" aria-label="Search results" aria-live="polite">
                {body}
                {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
            </section>
        </Show>
    }
}
