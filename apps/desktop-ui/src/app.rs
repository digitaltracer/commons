use std::collections::HashSet;

use leptos::prelude::*;
use proto::model::ConversationKind;
use proto::view::{AppEvent, ConnectionState, ConversationView, DraftReason, SidebarView};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::ipc;
use crate::messages::{Composer, EditBar, MessageList};
use crate::panels::{Members, Person, Prefs, SignInAgain, Tools};
use crate::search::SearchView;
use crate::spike;
use crate::state::{Aside, Ctx, Mode, Overlay, Place, Selection, ctx, is_dm};
use crate::switcher::{Switcher, status_label};
use crate::ui::{self, Icon, avatar, icon, sized};

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
    let c = Ctx::new();
    provide_context(c);
    let loaded = RwSignal::new(false);

    let load_presence = move |s: &SidebarView| {
        for a in &s.accounts {
            let id = a.id;
            leptos::task::spawn_local(async move {
                if let Ok(list) = ipc::invoke::<Vec<Uuid>>("online", json!({"accountId": id})).await
                {
                    c.presence.update(|p| {
                        p.insert(id, list.into_iter().collect());
                    });
                }
            });
        }
    };

    leptos::task::spawn_local(async move {
        let r: Result<SidebarView, String> = ipc::invoke_with_channel(
            "subscribe_app",
            json!({}),
            "onEvent",
            move |ev: AppEvent| match ev {
                AppEvent::Sidebar { sidebar: s } => c.sidebar.set(s),
                AppEvent::Drafts { drafts: d } => c.drafts.set(d),
                AppEvent::Typing {
                    conversation_id,
                    names,
                    ..
                } => {
                    if c.selected.get_untracked().map(|s| s.conversation_id)
                        == Some(conversation_id)
                    {
                        c.typing.set(names);
                    }
                }
                AppEvent::Presence { account_id, online } => c.presence.update(|p| {
                    p.insert(account_id, online.into_iter().collect::<HashSet<_>>());
                }),
                AppEvent::Error { message } => ipc::warn(message),
            },
        )
        .await;
        if let Ok(s) = r {
            load_presence(&s);
            c.sidebar.set(s);
        }
        loaded.set(true);
        if let Ok(cfg) = ipc::invoke::<SpikeConfig>("spike_config", json!({})).await
            && let (true, Some(a), Some(conv)) = (cfg.enabled, cfg.account_id, cfg.conversation_id)
        {
            c.open(a, conv);
            spike::run(cfg.live_seconds);
        }
    });

    // Keep the sidebar on a real place: the first workspace, or the place's account.
    Effect::new(move |_| {
        let s = c.sidebar.get();
        let place = c.place.get_untracked();
        let valid = place.is_some_and(|p| {
            s.accounts.iter().any(|a| {
                a.id == p.account_id
                    && p.workspace_id
                        .is_none_or(|w| a.workspaces.iter().any(|x| x.id == w))
            })
        });
        let first_ws = |a: &proto::view::AccountView| a.workspaces.first().map(|w| w.id);
        if !valid {
            match s.accounts.first() {
                Some(a) => c.go_to_workspace(a.id, first_ws(a)),
                None => c.place.set(None),
            }
        } else if let Some(p) = place
            && p.workspace_id.is_none()
            && let Some(a) = s.accounts.iter().find(|a| a.id == p.account_id)
            && let Some(w) = first_ws(a)
        {
            c.go_to_workspace(a.id, Some(w));
        }
        if let Some(sel) = c.selected.get_untracked()
            && c.conversation(sel).is_none()
        {
            c.selected.set(None);
        }
    });

    Effect::new(move |prev: Option<Option<Selection>>| {
        let now = c.selected.get();
        if prev.flatten() != now {
            c.aside.set(Aside::None);
            c.typing.set(vec![]);
            c.editing.set(None);
        }
        if let Some(s) = now {
            ipc::command(
                "set_focus",
                json!({"accountId": s.account_id, "conversationId": s.conversation_id}),
            );
        }
        now
    });

    let _tick = set_interval_with_handle(
        move || c.now.set(js_sys::Date::now()),
        std::time::Duration::from_secs(1),
    );

    let _keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if !ev.meta_key() {
            return;
        }
        let k = ev.key();
        if k == "k" {
            ev.prevent_default();
            c.overlay.set(Overlay::None);
            c.mode.set(Mode::Search);
        } else if let Ok(n) = k.parse::<usize>()
            && (1..=9).contains(&n)
            && let Some((a, w)) = c.workspace_order().get(n - 1).copied()
        {
            ev.prevent_default();
            c.overlay.set(Overlay::None);
            c.go_to_workspace(a, Some(w));
        }
    });

    view! {
        <Show when=move || loaded.get() fallback=|| view! { <div class="win"><p class="empty">"Loading…"</p></div> }>
            <Show when=move || !c.sidebar.with(|s| s.accounts.is_empty()) && !c.adding.get()
                  fallback=move || view! { <SignIn /> }>
                <div class="win">
                    <div class="left">
                        <div class="tb" data-tauri-drag-region></div>
                        <div class="cols">
                            <Rail />
                            <Side />
                        </div>
                    </div>
                    {move || match (c.mode.get(), c.place.get()) {
                        (Mode::Search, Some(Place { account_id, workspace_id: Some(w) })) =>
                            view! { <SearchView account_id=account_id workspace_id=w /> }.into_any(),
                        _ => match c.selected.get() {
                            Some(sel) => view! { <Conversation sel=sel /> }.into_any(),
                            None => view! { <Empty /> }.into_any(),
                        },
                    }}
                    {move || match (c.mode.get(), c.selected.get(), c.aside.get()) {
                        (Mode::Conversation, Some(sel), Aside::Thread(root)) => view! { <Thread sel=sel root=root /> }.into_any(),
                        (Mode::Conversation, Some(sel), Aside::Members) => view! { <Members sel=sel /> }.into_any(),
                        _ => ().into_any(),
                    }}
                    {move || overlay(c.overlay.get())}
                </div>
            </Show>
        </Show>
    }
}

fn overlay(o: Overlay) -> AnyView {
    let c = ctx();
    let here = c.place.get_untracked();
    match o {
        Overlay::None => ().into_any(),
        Overlay::Switcher => view! { <Switcher /> }.into_any(),
        Overlay::Prefs(a) => view! { <Prefs account_id=a /> }.into_any(),
        Overlay::SignInAgain(a) => view! { <SignInAgain account_id=a /> }.into_any(),
        Overlay::Person {
            account_id,
            user_id,
        } => view! { <Person account_id=account_id user_id=user_id /> }.into_any(),
        which @ (Overlay::NewChannel | Overlay::Browse | Overlay::NewDm) => match here {
            Some(Place {
                account_id,
                workspace_id: Some(w),
            }) => view! { <Tools account_id=account_id workspace_id=w which=which /> }.into_any(),
            _ => ().into_any(),
        },
    }
}

/// Accounts, each a group of its workspaces.
#[component]
fn Rail() -> impl IntoView {
    let c = ctx();
    view! {
        <nav class="rail" aria-label="Accounts and workspaces">
            {move || c.sidebar.get().accounts.into_iter().map(|a| {
                let account_id = a.id;
                let (status, _) = status_label(&a);
                let host = ui::host(&a.server_url);
                let off = matches!(a.connection, ConnectionState::Offline | ConnectionState::Connecting);
                let out = a.connection == ConnectionState::SignedOut;
                let tiles = if a.workspaces.is_empty() {
                    let current = move || c.place.get().is_some_and(|p| p.account_id == account_id);
                    view! {
                        <button class="ws" class:on=current style=format!("background: {}", ui::color(account_id))
                            aria-label=format!("{host}, no workspaces yet")
                            on:click=move |_| c.go_to_workspace(account_id, None)>{sized(Icon::Server, 18)}</button>
                    }.into_any()
                } else {
                    a.workspaces.into_iter().map(|w| {
                        let wid = w.id;
                        let current = move || c.place.get().is_some_and(|p| p.account_id == account_id && p.workspace_id == Some(wid));
                        let mentions: u32 = w.conversations.iter().map(|cv| cv.mentions).sum();
                        let unread = w.conversations.iter().any(|cv| cv.unread > 0 && !cv.muted);
                        let mut label = w.name.clone();
                        if mentions > 0 { label.push_str(&format!(", {mentions} mentions")); } else if unread { label.push_str(", unread"); }
                        if off { label.push_str(", reconnecting"); }
                        if out { label.push_str(", signed out"); }
                        view! {
                            <button class="ws" class:on=current class:dim=out style=format!("background: {}", ui::color(wid))
                                aria-label=move || if current() { format!("{label}, current") } else { label.clone() }
                                aria-expanded=move || (current() && c.overlay.get() == Overlay::Switcher).to_string()
                                on:click=move |_| if current() { c.overlay.set(Overlay::Switcher) } else { c.go_to_workspace(account_id, Some(wid)) }>
                                {ui::tile(&w.name)}
                                {if out {
                                    Some(view! { <span class="walert">{sized(Icon::Alert, 11)}</span> }.into_any())
                                } else if off {
                                    Some(view! { <span class="woff">{sized(Icon::Refresh, 11)}</span> }.into_any())
                                } else if mentions > 0 {
                                    Some(view! { <span class="wbadge">{mentions}</span> }.into_any())
                                } else if unread {
                                    Some(view! { <span class="wdot"></span> }.into_any())
                                } else { None }}
                            </button>
                        }
                    }).collect_view().into_any()
                };
                view! { <div class="srv" title=format!("{host}: {}", status.to_lowercase())>{tiles}</div> }
            }).collect_view()}
            <button class="addws" aria-label="Add a server account" on:click=move |_| c.adding.set(true)>{icon(Icon::Plus)}</button>
        </nav>
    }
}

fn conversation_icon(cv: &ConversationView) -> AnyView {
    match cv.kind {
        ConversationKind::Public => icon(Icon::Hash).into_any(),
        ConversationKind::Private => icon(Icon::Lock).into_any(),
        ConversationKind::Dm => {
            let id = cv.peer_ids.first().copied().unwrap_or(cv.id);
            avatar(id, &cv.title, "s").into_any()
        }
        ConversationKind::GroupDm => view! {
            <span class="av s" style="background: #6B6F6D" aria-hidden="true">{cv.peer_ids.len() + 1}</span>
        }
        .into_any(),
    }
}

/// The current workspace's conversations.
#[component]
fn Side() -> impl IntoView {
    let c = ctx();
    let account = move || c.place.get().and_then(|p| c.account(p.account_id));
    let workspace = move || {
        c.place
            .get()
            .and_then(|p| p.workspace_id.and_then(|w| c.workspace(p.account_id, w)))
    };
    let new_ws = RwSignal::new(String::new());

    let row = move |account_id: Uuid, cv: ConversationView| {
        let sel = Selection {
            account_id,
            conversation_id: cv.id,
        };
        let current = move || c.mode.get() == Mode::Conversation && c.selected.get() == Some(sel);
        let unread = cv.unread > 0 && !cv.muted;
        let mut label = cv.title.clone();
        if cv.mentions > 0 {
            label.push_str(&format!(", {} mentions", cv.mentions));
        } else if cv.unread > 0 {
            label.push_str(&format!(", {} unread", cv.unread));
        }
        if cv.muted {
            label.push_str(", muted");
        }
        if cv.pending > 0 {
            label.push_str(&format!(", {} waiting to send", cv.pending));
        }
        let peer = (cv.kind == ConversationKind::Dm)
            .then(|| cv.peer_ids.first().copied())
            .flatten();
        let title = cv.title.trim_start_matches('#').to_string();
        let count = if cv.mentions > 0 {
            cv.mentions
        } else if is_dm(cv.kind) {
            cv.unread
        } else {
            0
        };
        view! {
            <button class="row" class:unread=unread class:sel=current class:muted=cv.muted
                aria-current=move || if current() { "page" } else { "false" } aria-label=label
                on:click=move |_| c.open(account_id, sel.conversation_id)>
                {conversation_icon(&cv)}
                <span class="rt">{title}</span>
                {peer.map(|p| view! { <span class="pres" class:online=move || c.online(account_id, p)></span> })}
                {cv.muted.then(|| sized(Icon::BellOff, 13))}
                {(cv.pending > 0).then(|| view! { <span class="rpend" role="img" aria-label=format!("{} waiting to send", cv.pending)>{sized(Icon::Clock, 14)}</span> })}
                {(count > 0).then(|| view! { <span class="cnt">{count}</span> })}
            </button>
        }
    };

    view! {
        <div class="side">
            {move || account().map(|a| {
                let account_id = a.id;
                let host = ui::host(&a.server_url);
                let ws = workspace();
                let name = ws.as_ref().map(|w| w.name.clone()).unwrap_or_else(|| host.clone());
                let pending: u32 = a.workspaces.iter().flat_map(|w| &w.conversations).map(|cv| cv.pending).sum();
                let (status, dot) = match a.connection {
                    ConnectionState::Online => ("Active".to_string(), "online"),
                    ConnectionState::SignedOut => ("Signed out".to_string(), "out"),
                    _ if pending > 0 => (format!("Offline · {pending} {} waiting to send", if pending == 1 { "message" } else { "messages" }), "off"),
                    ConnectionState::Connecting => ("Connecting…".to_string(), "off"),
                    ConnectionState::Offline => ("Offline".to_string(), "off"),
                };
                let (channels, dms): (Vec<_>, Vec<_>) = ws.as_ref().map(|w| w.conversations.clone()).unwrap_or_default()
                    .into_iter().partition(|cv| !is_dm(cv.kind));
                view! {
                    <div class="sh">
                        <div style="flex: 1; min-width: 0">
                            <button class="wsname" aria-haspopup="dialog" on:click=move |_| c.overlay.set(Overlay::Switcher)>
                                {name}{sized(Icon::Chevron, 14)}</button>
                            <div class="hostline">{host}</div>
                        </div>
                        {ws.is_some().then(|| view! {
                            <button class="iconbtn" aria-label="New message" on:click=move |_| c.overlay.set(Overlay::NewDm)>{icon(Icon::Compose)}</button>
                        })}
                    </div>
                    <nav class="scroll" aria-label="Conversations">
                        {match ws {
                            None => view! {
                                <div class="sec">
                                    <p class="muted">"No workspaces on this server yet."</p>
                                    <form class="inline" on:submit=move |ev| {
                                        ev.prevent_default();
                                        let name = new_ws.get_untracked();
                                        if !name.trim().is_empty() {
                                            new_ws.set(String::new());
                                            ipc::command("create_workspace", json!({"accountId": account_id, "name": name}));
                                        }
                                    }>
                                        <input class="field" aria-label="New workspace name" placeholder="Workspace name"
                                            prop:value=move || new_ws.get() on:input=move |ev| new_ws.set(event_target_value(&ev)) />
                                        <button type="submit" class="btn primary">"Create"</button>
                                    </form>
                                </div>
                            }.into_any(),
                            Some(_) => view! {
                                <div style="padding: 0 8px">
                                    <button class="row" class:sel=move || c.mode.get() == Mode::Search
                                        on:click=move |_| c.mode.set(Mode::Search)>{icon(Icon::Search)}<span class="rt">"Search"</span><span class="kbd">"⌘K"</span></button>
                                </div>
                                <div class="sec">
                                    <div class="sect"><span>"Channels"</span>
                                        <button class="iconbtn xs" aria-label="Browse channels" on:click=move |_| c.overlay.set(Overlay::Browse)>{sized(Icon::Search, 12)}</button>
                                        <button class="iconbtn xs" aria-label="Create a channel" on:click=move |_| c.overlay.set(Overlay::NewChannel)>{sized(Icon::Plus, 12)}</button>
                                    </div>
                                    {channels.into_iter().map(|cv| row(account_id, cv)).collect_view()}
                                </div>
                                <div class="sec">
                                    <div class="sect"><span>"Direct messages"</span>
                                        <button class="iconbtn xs" aria-label="New direct message" on:click=move |_| c.overlay.set(Overlay::NewDm)>{sized(Icon::Plus, 12)}</button>
                                    </div>
                                    {dms.into_iter().map(|cv| row(account_id, cv)).collect_view()}
                                </div>
                            }.into_any(),
                        }}
                    </nav>
                    <div class="foot">
                        {avatar(a.user_id, &a.display_name, "m")}
                        <div style="flex: 1; min-width: 0">
                            <div class="fname2">{a.display_name.clone()}</div>
                            <div class="fstat"><span class=format!("dot {dot}")></span>{status}</div>
                        </div>
                        <button class="iconbtn" aria-label="Notification preferences"
                            on:click=move |_| c.overlay.set(Overlay::Prefs(account_id))>{icon(Icon::Bell)}</button>
                    </div>
                }
            })}
        </div>
    }
}

#[component]
fn Empty() -> impl IntoView {
    view! {
        <main class="main">
            <header class="hdr" data-tauri-drag-region></header>
            <div class="emptymain"><p class="muted">"Pick a conversation, or start one."</p></div>
        </main>
    }
}

/// The reconnecting or signed-out banner for the open conversation's account.
#[component]
fn Banner(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    move || {
        let a = c.account(account_id)?;
        let host = ui::host(&a.server_url);
        Some(match a.connection {
            ConnectionState::Online => return None,
            ConnectionState::SignedOut => view! {
                <div class="banner" role="status">
                    {icon(Icon::Alert)}
                    <span style="flex: 1"><strong>{format!("Your session on {host} ended.")}</strong>
                        " You're reading saved messages. Sign in again to send and receive."</span>
                    <button class="bbtn" on:click=move |_| c.overlay.set(Overlay::SignInAgain(account_id))>"Sign in"</button>
                </div>
            }.into_any(),
            _ => {
                let wait = a.reconnect_at_ms.map(|at| ((at as f64 - c.now.get()) / 1000.0).ceil().max(0.0) as i64);
                let when = match wait {
                    Some(s) if s > 0 => format!("Reconnecting in {s} {}.", if s == 1 { "second" } else { "seconds" }),
                    _ => "Reconnecting now.".to_string(),
                };
                view! {
                    <div class="banner" role="status">
                        {icon(Icon::Offline)}
                        <span style="flex: 1"><strong>{format!("Can't reach {host}.")}</strong>
                            {format!(" {when} You're reading saved messages, and new ones will send when you're back.")}</span>
                        <button class="bbtn" on:click=move |_| ipc::command("reconnect_now", json!({"accountId": account_id}))>"Try now"</button>
                    </div>
                }.into_any()
            }
        })
    }
}

#[component]
fn Conversation(sel: Selection) -> impl IntoView {
    let c = ctx();
    let info = move || c.conversation(sel);
    let title = move || info().map(|(_, cv)| cv.title).unwrap_or_default();
    let kind = untrack(move || info().map(|(_, cv)| cv.kind)).unwrap_or(ConversationKind::Public);
    let label = untrack(title);
    let short = label.trim_start_matches('#').to_string();
    let count = RwSignal::new(None::<usize>);
    let query = RwSignal::new(String::new());
    let load_count = move || {
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": sel.account_id, "conversationId": sel.conversation_id});
            if let Ok(m) =
                ipc::invoke::<Vec<proto::model::Member>>("conversation_members", args).await
            {
                count.set(Some(m.len()));
            }
        });
    };
    load_count();
    Effect::new(move |prev: Option<Aside>| {
        let a = c.aside.get();
        if prev == Some(Aside::Members) && a != Aside::Members {
            load_count();
        }
        a
    });
    let sub = move || {
        let (_, cv) = info()?;
        Some(match cv.kind {
            ConversationKind::Private => "Private channel".to_string(),
            ConversationKind::Public => "Public channel".to_string(),
            ConversationKind::GroupDm => "Group message".to_string(),
            ConversationKind::Dm => {
                let peer = cv.peer_ids.first().copied()?;
                if c.online(sel.account_id, peer) {
                    "Active".into()
                } else {
                    "Away".into()
                }
            }
        })
    };
    let muted = move || info().map(|(_, cv)| cv.muted).unwrap_or(false);
    let ws_name = move || info().map(|(w, _)| w.name).unwrap_or_default();
    let head_icon = match kind {
        ConversationKind::Public => sized(Icon::Hash, 18).into_any(),
        ConversationKind::Private => sized(Icon::Lock, 18).into_any(),
        _ => sized(Icon::Members, 18).into_any(),
    };
    let list_label = format!("Messages in {short}");
    let message_label = format!("Message {label}");
    let drafts = move || {
        c.drafts
            .get()
            .into_iter()
            .filter(|d| d.account_id == sel.account_id && d.conversation_id == sel.conversation_id)
            .map(|d| {
                let why = match d.reason {
                    DraftReason::StaleEdit => {
                        "Your edit wasn't saved because the message changed. Here is your text:"
                    }
                    DraftReason::Deleted => {
                        "Your edit wasn't saved because the message was deleted. Here is your text:"
                    }
                    DraftReason::SendFailed => "This message wasn't sent:",
                };
                let id = d.id;
                view! {
                    <div class="draft" role="alert">
                        <p>{why}</p><pre class="pre">{d.body.clone()}</pre>
                        <button class="plink" on:click=move |_| ipc::command("discard_send",
                            json!({"accountId": sel.account_id, "id": id}))>"Dismiss"</button>
                    </div>
                }
            })
            .collect_view()
    };
    view! {
        <main class="main">
            <header class="hdr" data-tauri-drag-region>
                <span class="hicon">{head_icon}</span>
                <h1>{move || title().trim_start_matches('#').to_string()}</h1>
                <div class="hsub">{sub}</div>
                <button class="iconbtn" aria-pressed=move || muted().to_string()
                    aria-label=move || if muted() { "Unmute notifications" } else { "Mute notifications" }
                    title=move || if muted() { "Muted: only mentions notify" } else { "Mute" }
                    on:click=move |_| ipc::command("set_muted", json!({"accountId": sel.account_id,
                        "conversationId": sel.conversation_id, "muted": !muted()}))>
                    {move || if muted() { icon(Icon::BellOff).into_any() } else { icon(Icon::Bell).into_any() }}</button>
                <button class="iconbtn wide" aria-label=move || format!("{} members", count.get().unwrap_or(0))
                    aria-pressed=move || (c.aside.get() == Aside::Members).to_string()
                    on:click=move |_| c.aside.update(|a| *a = if *a == Aside::Members { Aside::None } else { Aside::Members })>
                    {icon(Icon::Members)}{move || count.get().map(|n| n.to_string())}</button>
                <form class="search" role="search" on:submit=move |ev| {
                    ev.prevent_default();
                    c.query.set(query.get_untracked());
                    c.mode.set(Mode::Search);
                }>
                    {sized(Icon::Search, 14)}
                    <input type="search" placeholder=move || format!("Search {}", ws_name()) aria-label=move || format!("Search {}", ws_name())
                        prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
                    <span class="kbd">"⌘K"</span>
                </form>
            </header>
            <Banner account_id=sel.account_id />
            {drafts}
            <MessageList account_id=sel.account_id conversation_id=sel.conversation_id label=list_label />
            <EditBar account_id=sel.account_id conversation_id=sel.conversation_id />
            <Composer account_id=sel.account_id conversation_id=sel.conversation_id
                label=message_label.clone() placeholder=message_label />
            <div class="typing" aria-live="polite">{move || {
                let t = c.typing.get();
                match t.len() {
                    0 => String::new(),
                    1 => format!("{} is typing…", t[0]),
                    _ => format!("{} are typing…", t.join(", ")),
                }
            }}</div>
        </main>
    }
}

#[component]
fn Thread(sel: Selection, root: Uuid) -> impl IntoView {
    let c = ctx();
    let title = move || {
        c.conversation(sel)
            .map(|(_, cv)| cv.title)
            .unwrap_or_default()
    };
    view! {
        <aside class="thread" aria-label="Thread">
            <header class="hdr" data-tauri-drag-region style="padding: 0 10px 0 16px">
                <h2>"Thread"</h2>
                <span class="sub">{title}</span>
                <button class="iconbtn" aria-label="Close thread" on:click=move |_| c.aside.set(Aside::None)>{icon(Icon::Close)}</button>
            </header>
            <MessageList account_id=sel.account_id conversation_id=sel.conversation_id thread_root_id=root label="Thread messages" />
            <Composer account_id=sel.account_id conversation_id=sel.conversation_id
                thread_root_id=root label="Reply in thread" placeholder="Reply…" />
            <div class="typing"></div>
        </aside>
    }
}

#[component]
fn SignIn() -> impl IntoView {
    let c = ctx();
    let server = RwSignal::new("http://127.0.0.1:8080".to_string());
    let email = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let register = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let can_cancel = move || !c.sidebar.with(|s| s.accounts.is_empty());
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        error.set(None);
        let display = register.get_untracked().then(|| name.get_untracked());
        let args = json!({"serverUrl": server.get_untracked(), "email": email.get_untracked(),
                          "password": password.get_untracked(), "displayName": display});
        leptos::task::spawn_local(async move {
            match ipc::invoke::<Uuid>("add_account", args).await {
                Ok(id) => {
                    c.adding.set(false);
                    c.go_to_workspace(id, None);
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="win signinwin">
            <div class="tb" data-tauri-drag-region></div>
            <form class="signform card" on:submit=submit>
                <div class="brand"><span class="logo" aria-hidden="true">"C"</span>
                    <h1>{move || if register.get() { "Create a Commons account" } else { "Sign in to Commons" }}</h1></div>
                <p class="lede">"Use the address of your community's Commons server."</p>
                <label class="flabel">"Server" <input class="field" type="url" required prop:value=move || server.get() on:input=move |ev| server.set(event_target_value(&ev)) /></label>
                <label class="flabel">"Email" <input class="field" type="email" required autocomplete="username" prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev)) /></label>
                <label class="flabel">"Password" <input class="field" type="password" required minlength="8" autocomplete="current-password" prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) /></label>
                <label class="chk"><input type="checkbox" prop:checked=move || register.get() on:change=move |ev| register.set(event_target_checked(&ev)) />" Create a new account"</label>
                <Show when=move || register.get()>
                    <label class="flabel">"Display name" <input class="field" required prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) /></label>
                </Show>
                {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                <button type="submit" class="btn primary wide" disabled=move || busy.get()>{move || if register.get() { "Create account" } else { "Sign in" }}</button>
                <Show when=can_cancel>
                    <button type="button" class="btn wide" on:click=move |_| c.adding.set(false)>"Cancel"</button>
                </Show>
            </form>
        </div>
    }
}
