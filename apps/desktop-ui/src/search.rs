//! Workspace search: replaces the conversation while open. Results come newest first, which
//! is the only order the server offers (rank breaks ties).
use leptos::html::Input;
use leptos::prelude::*;
use proto::api::SearchHitKind;
use proto::view::SearchHitView;
use serde_json::json;
use uuid::Uuid;

use crate::ipc;
use crate::state::{Mode, ctx};
use crate::ui::{self, Icon, avatar, icon, sized};

/// The server's default page size.
const PAGE: usize = 20;

#[derive(Clone, Debug, PartialEq)]
enum Results {
    Idle,
    Searching,
    Done {
        query: String,
        hits: Vec<SearchHitView>,
        more: bool,
    },
    Failed(String),
}

#[component]
pub fn SearchView(account_id: Uuid, workspace_id: Uuid) -> impl IntoView {
    let c = ctx();
    let text = RwSignal::new(c.query.get_untracked());
    let results = RwSignal::new(Results::Idle);
    let loading_more = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let input: NodeRef<Input> = NodeRef::new();
    let ws_name = move || {
        c.workspace(account_id, workspace_id)
            .map(|w| w.name)
            .unwrap_or_default()
    };

    let fetch = move |q: String, offset: usize| async move {
        ipc::invoke::<Vec<SearchHitView>>(
            "search",
            json!({"accountId": account_id, "workspaceId": workspace_id, "query": q, "offset": offset}),
        )
        .await
    };
    let run = move || {
        let q = text.get_untracked().trim().to_string();
        c.query.set(q.clone());
        if q.is_empty() {
            results.set(Results::Idle);
            return;
        }
        results.set(Results::Searching);
        leptos::task::spawn_local(async move {
            results.set(match fetch(q.clone(), 0).await {
                Ok(hits) => Results::Done {
                    more: hits.len() >= PAGE,
                    query: q,
                    hits,
                },
                Err(e) => Results::Failed(e),
            });
        });
    };
    let more = move |_| {
        let Results::Done { query, hits, .. } = results.get_untracked() else {
            return;
        };
        loading_more.set(true);
        leptos::task::spawn_local(async move {
            if let Ok(next) = fetch(query.clone(), hits.len()).await {
                let more = next.len() >= PAGE;
                let mut all = hits;
                all.extend(next);
                results.set(Results::Done {
                    query,
                    hits: all,
                    more,
                });
            }
            loading_more.set(false);
        });
    };
    if !text.get_untracked().trim().is_empty() {
        run();
    }
    Effect::new(move |_| {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });
    let close = move || {
        c.mode.set(Mode::Conversation);
    };

    let open = move |hit: SearchHitView| {
        if hit.joined {
            c.open(account_id, hit.conversation_id);
            return;
        }
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": account_id, "conversationId": hit.conversation_id});
            match ipc::invoke::<serde_json::Value>("join_channel", args).await {
                Ok(_) => c.open_when_cached(account_id, hit.conversation_id),
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let card = move |hit: SearchHitView| {
        let snippet = hit
            .snippet
            .iter()
            .map(|p| {
                let t = p.text.clone();
                if p.hit {
                    view! { <mark>{t}</mark> }.into_any()
                } else {
                    t.into_any()
                }
            })
            .collect_view();
        let dm = !hit.conversation_title.starts_with('#');
        let place = hit.conversation_title.trim_start_matches('#').to_string();
        let mut notes = vec![];
        if hit.in_thread {
            notes.push("in a thread".to_string());
        }
        if dm {
            notes.push("direct message".into());
        }
        if !hit.joined {
            notes.push("not joined, opening joins".into());
        }
        let author = hit.author_name.clone().unwrap_or_else(|| "Unknown".into());
        let file = !matches!(hit.kind, SearchHitKind::Message);
        if file {
            notes.push(format!(
                "shared by {author} · {}",
                ui::when(hit.created_at_ms)
            ));
        }
        let notes = notes
            .into_iter()
            .map(|n| format!("· {n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let where_icon = if dm { Icon::Members } else { Icon::Hash };
        let lead = if file {
            view! { <span class="ficon">{sized(Icon::File, 18)}</span> }.into_any()
        } else {
            avatar(hit.author_id.unwrap_or_default(), &author, "").into_any()
        };
        let head = if file {
            let tag = match hit.kind {
                SearchHitKind::FileText => "Matched in file text",
                _ => "Matched in file name",
            };
            view! {
                <div class="mh"><span class="name mono">{hit.file_name.clone().unwrap_or_default()}</span>
                    <span class="tag">{tag}</span></div>
            }
            .into_any()
        } else {
            view! {
                <div class="mh"><span class="name">{author.clone()}</span>
                    <span class="time">{ui::when(hit.created_at_ms)}</span></div>
            }
            .into_any()
        };
        let label = format!(
            "{} in {}, {}",
            if file { "File" } else { "Message" },
            hit.conversation_title,
            ui::when(hit.created_at_ms)
        );
        let target = hit.clone();
        view! {
            <button class="res" aria-label=label on:click=move |_| open(target.clone())>
                {lead}
                <div class="rc">
                    <div class="where">{sized(where_icon, 13)}<b>{place}</b><span>{notes}</span></div>
                    {head}
                    <div class="snip">{snippet}</div>
                </div>
            </button>
        }
    };

    let body = move || {
        match results.get() {
        Results::Idle => view! { <p class="muted">"Type words to find, then press Return. Use quotes for a phrase and a minus sign to leave a word out."</p> }.into_any(),
        Results::Searching => view! { <p class="muted" role="status">"Searching…"</p> }.into_any(),
        Results::Failed(e) => view! { <p class="error" role="alert">{format!("Search failed: {e}")}</p> }.into_any(),
        Results::Done { query, hits, .. } if hits.is_empty() => {
            view! { <p class="muted" role="status">{format!("Nothing found for “{query}”.")}</p> }.into_any()
        }
        Results::Done { hits, more: has_more, .. } => {
            let n = hits.len();
            view! {
                <p class="count" role="status">{format!("{n}{} results", if has_more { "+" } else { "" })}</p>
                <div class="results">{hits.into_iter().map(card).collect_view()}</div>
                {has_more.then(|| view! {
                    <button class="btn more" on:click=more disabled=move || loading_more.get()>
                        {move || if loading_more.get() { "Loading…" } else { "Show more results" }}</button>
                })}
            }
            .into_any()
        }
    }
    };

    view! {
        <main class="main" aria-label="Search">
            <header class="hdr" data-tauri-drag-region style="gap: 12px">
                <form class="qfield" role="search" on:submit=move |ev| { ev.prevent_default(); run(); }>
                    {icon(Icon::Search)}
                    <input type="search" node_ref=input aria-label=move || format!("Search {}", ws_name())
                        placeholder=move || format!("Search {}", ws_name())
                        prop:value=move || text.get()
                        on:input=move |ev| text.set(event_target_value(&ev))
                        on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Escape" { close() } />
                    <button type="button" class="iconbtn sm" aria-label="Clear search"
                        on:click=move |_| { text.set(String::new()); results.set(Results::Idle); c.query.set(String::new()); }>
                        {sized(Icon::Close, 14)}</button>
                </form>
                <button class="iconbtn" aria-label="Close search" on:click=move |_| close()>{icon(Icon::Close)}</button>
            </header>
            <div class="sbody">
                <div class="scol">
                    <div class="srow">
                        <span class="sort">"Most recent first"</span>
                    </div>
                    <div class="info">
                        {icon(Icon::Info)}
                        <span>{move || format!("Searching channels you can read in {}. Chinese, Japanese and Korean text isn't searchable yet.", ws_name())}</span>
                    </div>
                    {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                    {body}
                </div>
            </div>
        </main>
    }
}
