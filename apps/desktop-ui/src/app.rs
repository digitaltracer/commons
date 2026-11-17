use leptos::prelude::*;
use proto::view::{AppEvent, ConnectionState, DraftReason, DraftView, MessageView, SidebarView};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::ipc;
use crate::messages::{Composer, MessageList};
use crate::search::{Results, SearchBox, SearchResults};
use crate::spike;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    pub account_id: Uuid,
    pub conversation_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpikeConfig {
    enabled: bool,
    account_id: Option<Uuid>,
    conversation_id: Option<Uuid>,
    live_seconds: u32,
}

#[component]
pub fn App() -> impl IntoView {
    let sidebar = RwSignal::new(SidebarView { accounts: vec![] });
    let loaded = RwSignal::new(false);
    let selected = RwSignal::new(None::<Selection>);
    let thread = RwSignal::new(None::<Uuid>);
    let drafts = RwSignal::new(Vec::<DraftView>::new());
    let typing = RwSignal::new(Vec::<String>::new());
    let adding = RwSignal::new(false);
    let editing = RwSignal::new(None::<MessageView>);

    leptos::task::spawn_local(async move {
        let r: Result<SidebarView, String> = ipc::invoke_with_channel(
            "subscribe_app",
            json!({}),
            "onEvent",
            move |ev: AppEvent| match ev {
                AppEvent::Sidebar { sidebar: s } => sidebar.set(s),
                AppEvent::Drafts { drafts: d } => drafts.set(d),
                AppEvent::Typing {
                    conversation_id,
                    names,
                    ..
                } => {
                    if selected.get_untracked().map(|s| s.conversation_id) == Some(conversation_id)
                    {
                        typing.set(names);
                    }
                }
                AppEvent::Presence { .. } => {}
                AppEvent::Error { message } => ipc::warn(message),
            },
        )
        .await;
        if let Ok(s) = r {
            sidebar.set(s);
        }
        loaded.set(true);
        if let Ok(cfg) = ipc::invoke::<SpikeConfig>("spike_config", json!({})).await
            && let (true, Some(a), Some(c)) = (cfg.enabled, cfg.account_id, cfg.conversation_id)
        {
            selected.set(Some(Selection {
                account_id: a,
                conversation_id: c,
            }));
            spike::run(cfg.live_seconds);
        }
    });

    Effect::new(move |prev: Option<Option<Selection>>| {
        let now = selected.get();
        if prev.flatten() != now {
            thread.set(None);
            typing.set(vec![]);
        }
        if let Some(s) = now {
            ipc::command(
                "set_focus",
                json!({"accountId": s.account_id, "conversationId": s.conversation_id}),
            );
        }
        now
    });

    view! {
        <Show when=move || loaded.get() fallback=|| view! { <p class="empty">"Loading…"</p> }>
            <Show when=move || !sidebar.with(|s| s.accounts.is_empty()) && !adding.get()
                  fallback=move || view! { <SignIn on_done=Callback::new(move |_| adding.set(false))
                                                    can_cancel=Signal::derive(move || !sidebar.with(|s| s.accounts.is_empty()))
                                                    on_cancel=Callback::new(move |_| adding.set(false)) /> }>
                <div class=move || if thread.get().is_some() { "app with-thread" } else { "app" }>
                    <Sidebar sidebar=sidebar selected=selected on_add=Callback::new(move |_| adding.set(true)) />
                    {move || match selected.get() {
                        None => view! { <main class="conversation"><p class="empty">"Pick a conversation."</p></main> }.into_any(),
                        Some(sel) => {
                            // Read the sidebar only inside these closures, so its updates never
                            // remount the message list.
                            let title = move || sidebar.with(|s| s.accounts.iter()
                                .find(|a| a.id == sel.account_id)
                                .and_then(|a| a.workspaces.iter().flat_map(|w| &w.conversations).find(|c| c.id == sel.conversation_id))
                                .map(|c| c.title.clone())).unwrap_or_default();
                            let label = format!("Message {}", untrack(title));
                            let workspace_id = untrack(|| sidebar.with(|s| s.accounts.iter()
                                .find(|a| a.id == sel.account_id)
                                .and_then(|a| a.workspaces.iter().find(|w| w.conversations.iter().any(|c| c.id == sel.conversation_id)))
                                .map(|w| w.id)));
                            let results = RwSignal::new(Results::Idle);
                            view! {
                                <main class="conversation">
                                    <header class="bar"><h1>{title}</h1>
                                        {workspace_id.map(|w| view! { <SearchBox account_id=sel.account_id workspace_id=w results=results /> })}
                                    </header>
                                    <SearchResults account_id=sel.account_id results=results selected=selected />
                                    <Drafts drafts=drafts selected=sel />
                                    <EditBar editing=editing account_id=sel.account_id conversation_id=sel.conversation_id />
                                    <MessageList account_id=sel.account_id conversation_id=sel.conversation_id
                                        on_thread=Callback::new(move |id| thread.set(Some(id)))
                                        on_edit=Callback::new(move |m| editing.set(Some(m))) />
                                    <p class="typing" aria-live="polite">{move || {
                                        let t = typing.get();
                                        if t.is_empty() { String::new() } else { format!("{} typing…", t.join(", ")) }
                                    }}</p>
                                    <Composer account_id=sel.account_id conversation_id=sel.conversation_id
                                        label=label />
                                </main>
                            }.into_any()
                        }
                    }}
                    {move || match (selected.get(), thread.get()) {
                        (Some(sel), Some(root)) => view! {
                            <aside class="thread" aria-label="Thread">
                                <header class="bar"><h1>"Thread"</h1>
                                    <button class="linkish" on:click=move |_| thread.set(None)>"Close"</button></header>
                                <MessageList account_id=sel.account_id conversation_id=sel.conversation_id thread_root_id=root />
                                <Composer account_id=sel.account_id conversation_id=sel.conversation_id
                                    thread_root_id=root label="Reply" />
                            </aside>
                        }.into_any(),
                        _ => ().into_any(),
                    }}
                </div>
            </Show>
        </Show>
    }
}

#[component]
fn Sidebar(
    sidebar: RwSignal<SidebarView>,
    selected: RwSignal<Option<Selection>>,
    on_add: Callback<()>,
) -> impl IntoView {
    let new_ws = RwSignal::new(String::new());
    view! {
        <nav class="sidebar" aria-label="Conversations">
            {move || sidebar.get().accounts.into_iter().map(|a| {
                let account_id = a.id;
                let status = match a.connection {
                    ConnectionState::Online => "online",
                    ConnectionState::Connecting => "connecting…",
                    ConnectionState::Offline => "offline",
                    ConnectionState::SignedOut => "signed out",
                };
                view! {
                    <section aria-label=format!("{} on {}", a.email, a.server_url)>
                        <h2>{a.server_url.clone()}</h2>
                        <p class="status">{format!("{} · {status}", a.display_name)}</p>
                        {a.workspaces.into_iter().map(|w| view! {
                            <h3>{w.name.clone()}</h3>
                            <WorkspaceTools account_id=account_id workspace_id=w.id selected=selected />
                            <ul>
                                {w.conversations.into_iter().map(|c| {
                                    let sel = Selection { account_id, conversation_id: c.id };
                                    let current = move || selected.get() == Some(sel);
                                    let unread = c.unread > 0;
                                    let label = if c.unread > 0 { format!("{}, {} unread", c.title, c.unread) } else { c.title.clone() };
                                    view! {
                                        <li><button class:unread=unread aria-current=move || current().to_string()
                                                aria-label=label on:click=move |_| selected.set(Some(sel))>
                                            <span>{c.title.clone()}</span>
                                            {(c.mentions > 0).then(|| view! { <span class="badge">{c.mentions}</span> })}
                                        </button></li>
                                    }
                                }).collect_view()}
                            </ul>
                        }).collect_view()}
                        <form on:submit=move |ev| {
                            ev.prevent_default();
                            let name = new_ws.get_untracked();
                            if !name.trim().is_empty() {
                                new_ws.set(String::new());
                                ipc::command("create_workspace", json!({"accountId": account_id, "name": name}));
                            }
                        }>
                            <label class="visually-hidden" for=format!("ws-{account_id}")>"New workspace name"</label>
                            <input id=format!("ws-{account_id}") placeholder="New workspace" prop:value=move || new_ws.get()
                                on:input=move |ev| new_ws.set(event_target_value(&ev)) />
                        </form>
                    </section>
                }
            }).collect_view()}
            <button class="linkish" on:click=move |_| on_add.run(())>"Add an account…"</button>
        </nav>
    }
}

#[component]
fn Drafts(drafts: RwSignal<Vec<DraftView>>, selected: Selection) -> impl IntoView {
    view! {
        {move || drafts.get().into_iter()
            .filter(|d| d.account_id == selected.account_id && d.conversation_id == selected.conversation_id)
            .map(|d| {
                let why = match d.reason {
                    DraftReason::StaleEdit => "Your edit wasn't saved because the message changed. Here is your text:",
                    DraftReason::Deleted => "Your edit wasn't saved because the message was deleted. Here is your text:",
                    DraftReason::SendFailed => "This message wasn't sent:",
                };
                let id = d.id;
                view! {
                    <div class="draft" role="alert">
                        <p>{why}</p><pre>{d.body.clone()}</pre>
                        <button class="linkish" on:click=move |_| ipc::command("discard_send",
                            json!({"accountId": selected.account_id, "id": id}))>"Dismiss"</button>
                    </div>
                }
            }).collect_view()}
    }
}

#[component]
fn EditBar(
    editing: RwSignal<Option<MessageView>>,
    account_id: Uuid,
    conversation_id: Uuid,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    Effect::new(move |_| {
        if let Some(m) = editing.get() {
            text.set(m.body.clone());
        }
    });
    view! {
        {move || editing.get().map(|m| view! {
            <form class="composer" on:submit=move |ev| {
                ev.prevent_default();
                ipc::command("edit_message", json!({"accountId": account_id, "conversationId": conversation_id,
                    "messageId": m.id, "baseRevision": m.revision, "body": text.get_untracked()}));
                editing.set(None);
            }>
                <label class="visually-hidden" for="edit-box">"Edit message"</label>
                <textarea id="edit-box" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))></textarea>
                <button type="submit" class="primary">"Save"</button>
                <button type="button" on:click=move |_| editing.set(None)>"Cancel"</button>
            </form>
        })}
    }
}

#[component]
fn SignIn(
    on_done: Callback<()>,
    can_cancel: Signal<bool>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let server = RwSignal::new("http://127.0.0.1:8080".to_string());
    let email = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let register = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        error.set(None);
        let display = register.get_untracked().then(|| name.get_untracked());
        let args = json!({"serverUrl": server.get_untracked(), "email": email.get_untracked(),
                          "password": password.get_untracked(), "displayName": display});
        leptos::task::spawn_local(async move {
            match ipc::invoke::<serde_json::Value>("add_account", args).await {
                Ok(_) => on_done.run(()),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <form class="signin" on:submit=submit>
            <h1>"Sign in to Commons"</h1>
            <label>"Server" <input type="url" required prop:value=move || server.get() on:input=move |ev| server.set(event_target_value(&ev)) /></label>
            <label>"Email" <input type="email" required autocomplete="username" prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev)) /></label>
            <label>"Password" <input type="password" required minlength="8" autocomplete="current-password" prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) /></label>
            <label><input type="checkbox" prop:checked=move || register.get() on:change=move |ev| register.set(event_target_checked(&ev)) />" Create a new account"</label>
            <Show when=move || register.get()>
                <label>"Display name" <input required prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) /></label>
            </Show>
            {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
            <button type="submit" class="primary" disabled=move || busy.get()>{move || if register.get() { "Create account" } else { "Sign in" }}</button>
            <Show when=move || can_cancel.get()>
                <button type="button" class="linkish" on:click=move |_| on_cancel.run(())>"Cancel"</button>
            </Show>
        </form>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    None,
    NewChannel,
    Browse,
    Dm,
}

/// Create a channel, browse and join channels, or start a DM in one workspace.
#[component]
fn WorkspaceTools(
    account_id: Uuid,
    workspace_id: Uuid,
    selected: RwSignal<Option<Selection>>,
) -> impl IntoView {
    let tool = RwSignal::new(Tool::None);
    let name = RwSignal::new(String::new());
    let private = RwSignal::new(false);
    let listed = RwSignal::new(Vec::<proto::model::Conversation>::new());
    let members = RwSignal::new(Vec::<proto::model::Member>::new());
    let error = RwSignal::new(None::<String>);
    let open = move |t: Tool| {
        error.set(None);
        tool.set(if tool.get_untracked() == t {
            Tool::None
        } else {
            t
        });
        let args = json!({"accountId": account_id, "workspaceId": workspace_id});
        match tool.get_untracked() {
            Tool::Browse => leptos::task::spawn_local(async move {
                match ipc::invoke("browse_channels", args).await {
                    Ok(l) => listed.set(l),
                    Err(e) => error.set(Some(e)),
                }
            }),
            Tool::Dm => leptos::task::spawn_local(async move {
                match ipc::invoke("workspace_members", args).await {
                    Ok(m) => members.set(m),
                    Err(e) => error.set(Some(e)),
                }
            }),
            _ => {}
        }
    };
    let go_to = move |c: proto::model::Conversation| {
        tool.set(Tool::None);
        selected.set(Some(Selection {
            account_id,
            conversation_id: c.id,
        }));
    };
    view! {
        <div class="tools">
            <button class="linkish" on:click=move |_| open(Tool::NewChannel)>"+ Channel"</button>
            <button class="linkish" on:click=move |_| open(Tool::Browse)>"Browse"</button>
            <button class="linkish" on:click=move |_| open(Tool::Dm)>"+ DM"</button>
            {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
            {move || match tool.get() {
                Tool::NewChannel => view! {
                    <form on:submit=move |ev| {
                        ev.prevent_default();
                        let args = json!({"accountId": account_id, "workspaceId": workspace_id,
                            "name": name.get_untracked(), "private": private.get_untracked()});
                        leptos::task::spawn_local(async move {
                            match ipc::invoke::<proto::model::Conversation>("create_channel", args).await {
                                Ok(c) => { name.set(String::new()); go_to(c) }
                                Err(e) => error.set(Some(e)),
                            }
                        });
                    }>
                        <label>"Name " <input required prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) /></label>
                        <label><input type="checkbox" prop:checked=move || private.get() on:change=move |ev| private.set(event_target_checked(&ev)) />" Private"</label>
                        <button type="submit" class="primary">"Create"</button>
                    </form>
                }.into_any(),
                Tool::Browse => view! {
                    <ul aria-label="Channels you can join">
                        {move || listed.get().into_iter().map(|c| {
                            let c2 = c.clone();
                            view! { <li><button on:click=move |_| {
                                let c = c2.clone();
                                let args = json!({"accountId": account_id, "conversationId": c.id});
                                leptos::task::spawn_local(async move {
                                    match ipc::invoke::<serde_json::Value>("join_channel", args).await {
                                        Ok(_) => go_to(c),
                                        Err(e) => error.set(Some(e)),
                                    }
                                });
                            }>{format!("Join #{}", c.name.clone().unwrap_or_default())}</button></li> }
                        }).collect_view()}
                    </ul>
                }.into_any(),
                Tool::Dm => view! {
                    <ul aria-label="Start a direct message">
                        {move || members.get().into_iter().map(|m| {
                            let uid = m.user_id;
                            view! { <li><button on:click=move |_| {
                                let args = json!({"accountId": account_id, "workspaceId": workspace_id, "userIds": [uid]});
                                leptos::task::spawn_local(async move {
                                    match ipc::invoke::<proto::model::Conversation>("open_dm", args).await {
                                        Ok(c) => go_to(c),
                                        Err(e) => error.set(Some(e)),
                                    }
                                });
                            }>{m.display_name.clone()}</button></li> }
                        }).collect_view()}
                    </ul>
                }.into_any(),
                Tool::None => ().into_any(),
            }}
        </div>
    }
}
