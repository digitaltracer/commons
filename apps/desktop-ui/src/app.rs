use std::collections::HashSet;

use leptos::prelude::*;
use proto::model::{ConversationKind, Role};
use proto::view::{AppEvent, ConnectionState, ConversationView, DraftReason, SidebarView};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::ipc;
use crate::jump::{Jump, Shortcuts};
use crate::layout::{self, Grip, Pane};
use crate::messages::{Composer, MessageList, focus_composer};
use crate::panels::{
    InvitePeople, JoinInvite, Members, Person, Prefs, SESSION_PASSWORD, SessionCard, SignInAgain,
    Tools,
};
use crate::search::SearchView;
use crate::settings::{AccountSettings, ProviderButton, WorkspaceAdmin, settings_button};
use crate::spike;
use crate::state::{Aside, Ctx, Mode, Overlay, Place, Selection, ctx, is_dm};
use crate::switcher::{Switcher, status_label};
use crate::ui::{self, Icon, avatar, icon, sized};

/// The header's search field, which ⌘F focuses.
const SEARCH_FIELD: &str = "hdr-search";

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
    let update = RwSignal::new(None::<String>);

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
                AppEvent::UpdateReady { version } => update.set(Some(version)),
                AppEvent::Staged { .. }
                | AppEvent::FileDrag { .. }
                | AppEvent::FilesDropped { .. } => {}
            },
        )
        .await;
        if let Ok(s) = r {
            load_presence(&s);
            c.sidebar.set(s);
        }
        loaded.set(true);
        if let Ok(Some(v)) = ipc::invoke::<Option<String>>("update_ready", json!({})).await {
            update.set(Some(v));
        }
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
        let before = prev.flatten();
        if before != now {
            c.aside.set(Aside::None);
            c.typing.set(vec![]);
            // An edit left open waits for its conversation to come back.
            if let (Some(b), Some(m)) = (before, c.editing.get_untracked()) {
                let t = c.edit_text.get_untracked();
                c.edits.update_value(|e| {
                    e.insert(b.conversation_id, (m, t));
                });
            }
            match now.and_then(|n| {
                c.edits
                    .try_update_value(|e| e.remove(&n.conversation_id))
                    .flatten()
            }) {
                Some((m, t)) => {
                    c.edit_text.set(t);
                    c.editing.set(Some(m));
                }
                None => c.editing.set(None),
            }
        }
        if let Some(s) = now {
            ipc::command(
                "set_focus",
                json!({"accountId": s.account_id, "conversationId": s.conversation_id}),
            );
        }
        now
    });

    // A conversation left open behind other windows isn't being read: tell the core.
    let set_active = move |active: bool| {
        if c.active.get_untracked() != active {
            c.active.set(active);
            ipc::command("set_window_active", json!({ "active": active }));
        }
    };
    let visible = || !document().hidden();
    let _focus = window_event_listener(leptos::ev::focus, move |_| set_active(visible()));
    let _blur = window_event_listener(leptos::ev::blur, move |_| set_active(false));
    let _vis = window_event_listener_untyped("visibilitychange", move |_| {
        set_active(visible() && document().has_focus().unwrap_or(true));
    });
    set_active(visible() && document().has_focus().unwrap_or(true));

    leptos::task::spawn_local(async move {
        if let Ok(p) = ipc::invoke::<proto::view::UiPrefs>("ui_prefs", json!({})).await {
            c.prefs.set(p);
        }
    });
    let _resize = window_event_listener(leptos::ev::resize, move |_| {
        if let Some(w) = window().inner_width().ok().and_then(|v| v.as_f64()) {
            c.win_width.set(w);
        }
    });
    let layout_vars = move || {
        let p = c.prefs.get();
        let w = c.win_width.get();
        format!(
            "--sw: {}px; --tw: {}px",
            layout::sidebar_width(p, w),
            p.thread()
        )
    };
    let aside_open = move || {
        c.mode.get() == Mode::Conversation
            && c.selected.get().is_some()
            && c.aside.get() != Aside::None
    };
    let narrow = move || layout::thread_overlays(c.prefs.get(), c.win_width.get());

    let _tick = set_interval_with_handle(
        move || c.now.set(js_sys::Date::now()),
        std::time::Duration::from_secs(1),
    );

    let _keys = window_event_listener(leptos::ev::keydown, move |ev| {
        // Escape closes the side panel unless something on the way up used it.
        if ev.key() == "Escape"
            && !ev.default_prevented()
            && c.overlay.get_untracked() == Overlay::None
            && c.aside.get_untracked() != Aside::None
        {
            ev.prevent_default();
            c.aside.set(Aside::None);
            if let Some(sel) = c.selected.get_untracked() {
                focus_composer(sel.conversation_id, false);
            }
            return;
        }
        // ⌥↑/⌥↓ walk the sidebar; with ⇧, only unread conversations.
        if ev.alt_key()
            && !ev.meta_key()
            && !ev.ctrl_key()
            && matches!(ev.key().as_str(), "ArrowUp" | "ArrowDown")
            && c.overlay.get_untracked() == Overlay::None
        {
            let up = ev.key() == "ArrowUp";
            let current = c.selected.get_untracked();
            let list = if ev.shift_key() {
                c.unread_order()
            } else {
                c.place
                    .get_untracked()
                    .and_then(|p| p.workspace_id.map(|w| c.sidebar_order(p.account_id, w)))
                    .unwrap_or_default()
            };
            let at = current.and_then(|s| list.iter().position(|x| *x == s));
            let next = match (at, up) {
                (Some(i), true) => i.checked_sub(1),
                (Some(i), false) => (i + 1 < list.len()).then_some(i + 1),
                // Not in the list (it's read): unread goes to the first one either way.
                (None, _) if ev.shift_key() => (!list.is_empty()).then_some(0),
                (None, true) => list.len().checked_sub(1),
                (None, false) => (!list.is_empty()).then_some(0),
            };
            if let Some(s) = next.and_then(|i| list.get(i)) {
                ev.prevent_default();
                c.open(s.account_id, s.conversation_id);
            }
            return;
        }
        if !ev.meta_key() {
            return;
        }
        let k = ev.key().to_lowercase();
        if k == "k" {
            ev.prevent_default();
            c.overlay.update(|o| {
                *o = if *o == Overlay::Jump {
                    Overlay::None
                } else {
                    Overlay::Jump
                }
            });
        } else if k == "f" && ev.shift_key() {
            ev.prevent_default();
            c.overlay.set(Overlay::None);
            c.mode.set(Mode::Search);
        } else if k == "f" {
            ev.prevent_default();
            c.overlay.set(Overlay::None);
            match document()
                .get_element_by_id(SEARCH_FIELD)
                .and_then(|e| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(e).ok())
            {
                Some(el) => {
                    let _ = el.focus();
                }
                None => c.mode.set(Mode::Search),
            }
        } else if k == "/" {
            ev.prevent_default();
            c.overlay.set(Overlay::Shortcuts);
        } else if let Ok(n) = k.parse::<usize>()
            && (1..=9).contains(&n)
            && let Some((a, w)) = c.workspace_order().get(n - 1).copied()
        {
            ev.prevent_default();
            c.overlay.set(Overlay::None);
            c.go_to_workspace(a, Some(w));
        }
    });

    let restarting = RwSignal::new(false);
    let restart = move |_| {
        restarting.set(true);
        leptos::task::spawn_local(async move {
            if let Err(e) = ipc::invoke::<()>("restart_to_update", json!({})).await {
                restarting.set(false);
                c.notice
                    .set(Some(format!("The update could not be installed: {e}")));
            }
        });
    };

    view! {
        {move || update.get().map(|v| view! {
            <div class="update" role="status">
                <span>{format!("Update available: Commons {v}")}</span>
                <button class="btn primary" disabled=move || restarting.get() on:click=restart>"Restart to update"</button>
                <button class="btn" disabled=move || restarting.get() on:click=move |_| update.set(None)>"Later"</button>
            </div>
        })}
        {move || c.notice.get().map(|n| view! {
            <div class="notice" role="status">
                <span>{n}</span>
                <button class="bbtn" on:click=move |_| c.notice.set(None)>"Dismiss"</button>
            </div>
        })}
        <Show when=move || loaded.get() fallback=|| view! { <div class="win"><p class="empty">"Loading…"</p></div> }>
            <Show when=move || !c.sidebar.with(|s| s.accounts.is_empty()) && !c.adding.get()
                  fallback=move || view! { <SignIn /> }>
                <div class="win" style=layout_vars>
                    <div class="left">
                        <div class="railcol">
                            <div class="tb" data-tauri-drag-region></div>
                            <Rail />
                        </div>
                        <div class="sidecol">
                            <div class="tb" data-tauri-drag-region></div>
                            <Side />
                            <Grip pane=Pane::Sidebar />
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
                    <Show when=aside_open>
                        // Too narrow for both columns: the panel floats over the messages.
                        {move || narrow().then(|| view! {
                            <div class="ascrim" aria-hidden="true" on:click=move |_| c.aside.set(Aside::None)></div>
                        })}
                        <div class="asidewrap" class:over=narrow>
                            {move || (!narrow()).then(|| view! { <Grip pane=Pane::Thread /> })}
                            {move || match (c.selected.get(), c.aside.get()) {
                                (Some(sel), Aside::Thread(root)) => view! { <Thread sel=sel root=root /> }.into_any(),
                                (Some(sel), Aside::Members) => view! { <Members sel=sel /> }.into_any(),
                                _ => ().into_any(),
                            }}
                        </div>
                    </Show>
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
        Overlay::Jump => view! { <Jump /> }.into_any(),
        Overlay::Shortcuts => view! { <Shortcuts /> }.into_any(),
        Overlay::Prefs(a) => view! { <Prefs account_id=a /> }.into_any(),
        Overlay::SignInAgain(a) => view! { <SignInAgain account_id=a /> }.into_any(),
        Overlay::Invite {
            account_id,
            workspace_id,
        } => view! { <InvitePeople account_id=account_id workspace_id=workspace_id /> }.into_any(),
        Overlay::JoinInvite => view! { <JoinInvite /> }.into_any(),
        Overlay::Account(a) => view! { <AccountSettings account_id=a /> }.into_any(),
        Overlay::Admin {
            account_id,
            workspace_id,
        } => {
            view! { <WorkspaceAdmin account_id=account_id workspace_id=workspace_id /> }.into_any()
        }
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
                            <button class="ws" class:on=current class:out=out style=format!("background: {}", ui::color(wid))
                                aria-label=move || if current() { format!("{label}, current") } else { label.clone() }
                                aria-expanded=move || (current() && c.overlay.get() == Overlay::Switcher).to_string()
                                on:click=move |_| if current() { c.overlay.set(Overlay::Switcher) } else { c.go_to_workspace(account_id, Some(wid)) }>
                                <span class="wt">{ui::tile(&w.name)}</span>
                                {if out {
                                    Some(view! { <span class="wlock">{sized(Icon::Lock, 10)}</span> }.into_any())
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
        if cv.has_draft {
            label.push_str(", draft");
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
                on:click=move |ev| {
                    // A click from Return or Space has no pointer detail; keep focus in the sidebar.
                    c.keyboard_open.set_value(ev.detail() == 0);
                    c.open(account_id, sel.conversation_id);
                }>
                {conversation_icon(&cv)}
                <span class="rt">{title}</span>
                {peer.map(|p| view! { <span class="pres" class:online=move || c.online(account_id, p)></span> })}
                {cv.muted.then(|| sized(Icon::BellOff, 13))}
                {(cv.pending > 0).then(|| view! { <span class="rpend" role="img" aria-label=format!("{} waiting to send", cv.pending)>{sized(Icon::Clock, 14)}</span> })}
                {(cv.has_draft && count == 0 && cv.pending == 0).then(|| view! { <span class="rdraft">{sized(Icon::Edit, 13)}</span> })}
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
                    ConnectionState::SignedOut if pending > 0 => (format!("Signed out · {pending} {} waiting to send", if pending == 1 { "message" } else { "messages" }), "out"),
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
                        {ws.as_ref().filter(|w| matches!(w.role, Some(Role::Owner | Role::Admin))).map(|w| {
                            let workspace_id = w.id;
                            view! {
                                <button class="iconbtn" aria-label="Manage workspace" title="Manage workspace"
                                    on:click=move |_| c.overlay.set(Overlay::Admin { account_id, workspace_id })>{icon(Icon::Shield)}</button>
                                <button class="iconbtn" aria-label="Invite people" title="Invite people"
                                    on:click=move |_| c.overlay.set(Overlay::Invite { account_id, workspace_id })>{icon(Icon::Members)}</button>
                            }
                        })}
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
                                    <button class="jumpbtn" aria-haspopup="dialog" aria-keyshortcuts="Meta+K"
                                        on:click=move |_| c.overlay.set(Overlay::Jump)>{sized(Icon::Search, 14)}<span class="rt">"Jump to…"</span><span class="kbd">"⌘K"</span></button>
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
                            <div class="fstat">{if dot == "out" { sized(Icon::Lock, 11).into_any() } else {
                                view! { <span class=format!("dot {dot}")></span> }.into_any() }}{status}</div>
                        </div>
                        <button class="iconbtn" aria-label="Notification preferences"
                            on:click=move |_| c.overlay.set(Overlay::Prefs(account_id))>{icon(Icon::Bell)}</button>
                        {settings_button(account_id)}
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

/// The reconnecting banner for the open conversation's account. Signed out shows a card instead.
#[component]
fn Banner(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    move || {
        let a = c.account(account_id)?;
        let host = ui::host(&a.server_url);
        Some(match a.connection {
            ConnectionState::Online => return None,
            ConnectionState::SignedOut => return None,
            _ => {
                let wait = a
                    .reconnect_at_ms
                    .map(|at| ((at as f64 - c.now.get()) / 1000.0).ceil().max(0.0) as i64);
                let when = match wait {
                    Some(s) if s > 0 => format!(
                        "Reconnecting in {s} {}.",
                        if s == 1 { "second" } else { "seconds" }
                    ),
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
    let members = RwSignal::new(None::<Vec<proto::model::Member>>);
    let count = move || members.with(|m| m.as_ref().map(|m| m.len()));
    let query = RwSignal::new(String::new());
    let load_count = move || {
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": sel.account_id, "conversationId": sel.conversation_id});
            if let Ok(m) =
                ipc::invoke::<Vec<proto::model::Member>>("conversation_members", args).await
            {
                c.remember_names(sel.account_id, &m);
                members.set(Some(m));
            }
        });
    };
    let faces = move || {
        members.with(|m| {
            m.iter()
                .flatten()
                .take(3)
                .map(|p| avatar(p.user_id, &p.display_name, "s"))
                .collect_view()
        })
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
            ConversationKind::Private | ConversationKind::Public | ConversationKind::GroupDm => {
                String::new()
            }
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
                {move || (c.connection(sel.account_id) == ConnectionState::SignedOut).then(|| view! {
                    <button class="outpill" aria-label="Signed out. Go to sign in" on:click=move |_| {
                        if let Some(el) = document().get_element_by_id(SESSION_PASSWORD)
                            .and_then(|e| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(e).ok()) {
                            let _ = el.focus();
                        }
                    }>{sized(Icon::Lock, 12)}"Signed out"</button>
                })}
                <button class="iconbtn" aria-pressed=move || muted().to_string()
                    aria-label=move || if muted() { "Unmute notifications" } else { "Mute notifications" }
                    title=move || if muted() { "Muted: only mentions notify" } else { "Mute" }
                    on:click=move |_| ipc::command("set_muted", json!({"accountId": sel.account_id,
                        "conversationId": sel.conversation_id, "muted": !muted()}))>
                    {move || if muted() { icon(Icon::BellOff).into_any() } else { icon(Icon::Bell).into_any() }}</button>
                <button class="face" aria-label=move || format!("{} members", count().unwrap_or(0))
                    aria-pressed=move || (c.aside.get() == Aside::Members).to_string()
                    on:click=move |_| c.aside.update(|a| *a = if *a == Aside::Members { Aside::None } else { Aside::Members })>
                    <span class="faces">{faces}</span>{move || count().map(|n| n.to_string())}</button>
                <form class="search" role="search" on:submit=move |ev| {
                    ev.prevent_default();
                    c.query.set(query.get_untracked());
                    c.mode.set(Mode::Search);
                }>
                    {sized(Icon::Search, 14)}
                    <input type="search" id=SEARCH_FIELD aria-keyshortcuts="Meta+F"
                        placeholder=move || format!("Search {}", ws_name()) aria-label=move || format!("Search {}", ws_name())
                        prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
                    <span class="kbd">"⌘F"</span>
                </form>
            </header>
            <Banner account_id=sel.account_id />
            {drafts}
            <MessageList account_id=sel.account_id conversation_id=sel.conversation_id label=list_label />
            <SessionCard account_id=sel.account_id />
            <Composer account_id=sel.account_id conversation_id=sel.conversation_id
                label=message_label.clone() placeholder=message_label />
            <div class="typing" aria-live="polite">{move || {
                let t = c.typing.get();
                let text = match t.len() {
                    0 => return None,
                    1 => format!("{} is typing…", t[0]),
                    _ => format!("{} are typing…", t.join(", ")),
                };
                Some(view! { <span class="dots" aria-hidden="true"><i></i><i></i><i></i></span>{text} })
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
    let invite = RwSignal::new(String::new());
    let can_cancel = move || !c.sidebar.with(|s| s.accounts.is_empty());
    // A pasted link names its server; fill it in.
    let set_invite = move |pasted: String| {
        if let (Some(url), _) = proto::api::split_invite(&pasted) {
            server.set(url);
        }
        invite.set(pasted);
    };
    if let Some(link) = c.invite_link.get_untracked() {
        c.invite_link.set(None);
        set_invite(link);
        register.set(true);
    }
    // Whether the server offers a provider, re-asked as the address settles.
    let provider = RwSignal::new(None::<String>);
    let oidc_busy = RwSignal::new(false);
    let asked = StoredValue::new(0u32);
    Effect::new(move |_| {
        let url = server.get();
        let n = asked.get_value() + 1;
        asked.set_value(n);
        provider.set(None);
        set_timeout(
            move || {
                if asked.get_value() != n {
                    return;
                }
                leptos::task::spawn_local(async move {
                    let meta =
                        ipc::invoke::<proto::api::Meta>("server_meta", json!({"serverUrl": url}))
                            .await;
                    if asked.get_value() == n {
                        provider.set(meta.ok().and_then(|m| m.oidc).map(|o| o.name));
                    }
                });
            },
            std::time::Duration::from_millis(400),
        );
    });
    let start_oidc = move || {
        oidc_busy.set(true);
        error.set(None);
        let token = invite
            .with_untracked(|i| (!i.trim().is_empty()).then(|| proto::api::split_invite(i).1));
        let args = json!({"serverUrl": server.get_untracked(), "invite": token});
        leptos::task::spawn_local(async move {
            match ipc::invoke::<Uuid>("oidc_sign_in", args).await {
                Ok(id) => {
                    c.adding.set(false);
                    c.go_to_workspace(id, None);
                }
                Err(e) => error.set(Some(e)),
            }
            oidc_busy.set(false);
        });
    };
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        error.set(None);
        let display = register.get_untracked().then(|| name.get_untracked());
        let token = invite
            .with_untracked(|i| (!i.trim().is_empty()).then(|| proto::api::split_invite(i).1));
        let args = json!({"serverUrl": server.get_untracked(), "email": email.get_untracked(),
                          "password": password.get_untracked(), "displayName": display,
                          "invite": token});
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
                <p class="lede">"Use the address of your community's Commons server, or paste an invite link."</p>
                <label class="flabel">"Invite link (optional)" <input class="field" placeholder="https://…/invite#…"
                    prop:value=move || invite.get() on:input=move |ev| set_invite(event_target_value(&ev)) /></label>
                <label class="flabel">"Server" <input class="field" type="url" required prop:value=move || server.get() on:input=move |ev| server.set(event_target_value(&ev)) /></label>
                {move || provider.get().map(|p| view! {
                    <ProviderButton name=p.clone() busy=oidc_busy label=format!("Continue with {p}") on_start=start_oidc />
                    <div class="divider" role="separator"><span>"or with email"</span></div>
                })}
                <label class="flabel">"Email" <input class="field" type="email" required autocomplete="username" prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev)) /></label>
                <label class="flabel">"Password" <input class="field" type="password" required minlength="8" autocomplete="current-password" prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) /></label>
                <label class="chk"><input type="checkbox" prop:checked=move || register.get() on:change=move |ev| register.set(event_target_checked(&ev)) />" Create a new account"</label>
                <Show when=move || register.get()>
                    <label class="flabel">"Display name" <input class="field" required prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) /></label>
                </Show>
                {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                <button type="submit" class="btn primary wide" disabled=move || busy.get() || oidc_busy.get()>{move || if register.get() { "Create account" } else { "Sign in" }}</button>
                <Show when=can_cancel>
                    <button type="button" class="btn wide" on:click=move |_| c.adding.set(false)>"Cancel"</button>
                </Show>
            </form>
        </div>
    }
}
