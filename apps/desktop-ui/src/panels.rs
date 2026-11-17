//! Side panels and dialogs: members, notification preferences, people, channel tools.
use leptos::prelude::*;
use proto::model::{Conversation, ConversationKind, Member, NotifyLevel, Preferences, Role};
use serde_json::json;
use uuid::Uuid;

use crate::ipc;
use crate::state::{Aside, Overlay, Selection, ctx, is_dm};
use crate::ui::{self, Icon, avatar, icon, sized};

/// A modal dialog over a scrim. Escape or a click on the scrim closes it.
#[component]
pub fn Dialog(#[prop(into)] label: String, children: Children) -> impl IntoView {
    let c = ctx();
    let aria = label.clone();
    view! {
        <div class="scrim" on:click=move |_| c.overlay.set(Overlay::None)></div>
        <section class="pop dlg" role="dialog" aria-modal="true" aria-label=aria
            on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Escape" { c.overlay.set(Overlay::None) }>
            <header class="dhead">
                <h2>{label}</h2>
                <button class="iconbtn" aria-label="Close" on:click=move |_| c.overlay.set(Overlay::None)>{icon(Icon::Close)}</button>
            </header>
            {children()}
        </section>
    }
}

fn presence_dot(online: bool) -> impl IntoView {
    view! { <span class="pres" class:online=online aria-label=if online { "Active" } else { "Away" } role="img"></span> }
}

/// The member list for the open conversation, with add, remove and leave.
#[component]
pub fn Members(sel: Selection) -> impl IntoView {
    let c = ctx();
    let members = RwSignal::new(Vec::<Member>::new());
    let error = RwSignal::new(None::<String>);
    let adding = RwSignal::new(false);
    let filter = RwSignal::new(String::new());
    let candidates = RwSignal::new(Vec::<Member>::new());
    let me = c.me(sel.account_id).unwrap_or_default();
    let info = move || c.conversation(sel);
    let kind = move || {
        info()
            .map(|(_, cv)| cv.kind)
            .unwrap_or(ConversationKind::Public)
    };
    let my_role = move || c.role(sel).unwrap_or(Role::Member);

    let load = move || {
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": sel.account_id, "conversationId": sel.conversation_id});
            match ipc::invoke::<Vec<Member>>("conversation_members", args).await {
                Ok(m) => {
                    error.set(None);
                    members.set(m)
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    load();
    let start_adding = move |_| {
        adding.set(true);
        let ws = info().map(|(w, _)| w.id);
        leptos::task::spawn_local(async move {
            if let Some(ws) = ws
                && let Ok(m) = ipc::invoke::<Vec<Member>>(
                    "workspace_members",
                    json!({"accountId": sel.account_id, "workspaceId": ws}),
                )
                .await
            {
                candidates.set(m);
            }
        });
    };
    let add = move |user_id: Uuid| {
        if kind() == ConversationKind::Dm {
            // A 1:1 DM stays private to its two people; adding someone starts a group DM.
            let Some((w, cv)) = info() else { return };
            let mut ids = cv.peer_ids.clone();
            ids.push(user_id);
            c.aside.set(Aside::None);
            c.open_dm(sel.account_id, w.id, ids);
            return;
        }
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": sel.account_id, "conversationId": sel.conversation_id, "userId": user_id});
            match ipc::invoke::<serde_json::Value>("add_member", args).await {
                Ok(_) => {
                    adding.set(false);
                    filter.set(String::new());
                    load();
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let remove = move |user_id: Uuid| {
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": sel.account_id, "conversationId": sel.conversation_id, "userId": user_id});
            match ipc::invoke::<serde_json::Value>("remove_member", args).await {
                Ok(_) => load(),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let leave = move |_| {
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": sel.account_id, "conversationId": sel.conversation_id});
            match ipc::invoke::<serde_json::Value>("leave_channel", args).await {
                Ok(_) => {
                    c.aside.set(Aside::None);
                    c.selected.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let can_remove = move || !is_dm(kind()) && my_role().reads_private_channels();
    let can_add = move || my_role() != Role::Guest || is_dm(kind());
    let muted = move || info().map(|(_, cv)| cv.muted).unwrap_or(false);

    let row = move |m: Member| {
        let uid = m.user_id;
        let online = move || c.online(sel.account_id, uid);
        let badge = match m.role {
            Role::Owner => Some("Owner"),
            Role::Admin => Some("Admin"),
            Role::Guest => Some("Guest"),
            Role::Member => None,
        };
        let name = m.display_name.clone();
        view! {
            <li class="mrow">
                <button class="mperson" on:click=move |_| c.overlay.set(Overlay::Person { account_id: sel.account_id, user_id: uid })>
                    {avatar(uid, &m.display_name, "m")}
                    <span class="mname">{m.display_name.clone()}{(uid == me).then_some(" (you)")}</span>
                    {move || presence_dot(online())}
                    {badge.map(|b| view! { <span class="tag">{b}</span> })}
                </button>
                {(uid != me && can_remove()).then(|| view! {
                    <button class="iconbtn" aria-label=format!("Remove {name}") on:click=move |_| remove(uid)>{icon(Icon::Close)}</button>
                })}
            </li>
        }
    };

    let title = move || info().map(|(_, cv)| cv.title).unwrap_or_default();
    view! {
        <aside class="thread" aria-label="Members">
            <header class="hdr" data-tauri-drag-region style="padding: 0 10px 0 16px">
                <h2>"Members"</h2>
                <span class="sub">{title}</span>
                <button class="iconbtn" aria-label="Close members" on:click=move |_| c.aside.set(Aside::None)>{icon(Icon::Close)}</button>
            </header>
            <div class="pbody">
                {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                <Show when=move || can_add()>
                    <Show when=move || adding.get() fallback=move || view! {
                        <button class="btn wide" on:click=start_adding>{icon(Icon::Plus)}"Add people"</button>
                    }>
                        <div class="find">
                            {sized(Icon::Search, 14)}
                            <input aria-label="Find people to add" placeholder="Find people to add" autofocus
                                prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
                        </div>
                        <ul class="mlist" aria-label="People you can add">
                            {move || {
                                let have: Vec<Uuid> = members.with(|m| m.iter().map(|x| x.user_id).collect());
                                let f = filter.get().to_lowercase();
                                candidates.get().into_iter()
                                    .filter(|m| !have.contains(&m.user_id) && m.display_name.to_lowercase().contains(&f))
                                    .take(20)
                                    .map(|m| {
                                        let uid = m.user_id;
                                        view! { <li class="mrow"><button class="mperson" on:click=move |_| add(uid)>
                                            {avatar(uid, &m.display_name, "m")}<span class="mname">{m.display_name.clone()}</span>
                                            <span class="plink">"Add"</span></button></li> }
                                    }).collect_view()
                            }}
                        </ul>
                        <button class="btn" on:click=move |_| adding.set(false)>"Done"</button>
                    </Show>
                </Show>
                <div class="sect">{move || format!("{} members", members.with(|m| m.len()))}</div>
                <ul class="mlist">{move || members.get().into_iter().map(row).collect_view()}</ul>
                <div class="pfoot2">
                    <label class="chk">
                        <input type="checkbox" prop:checked=muted
                            on:change=move |ev| ipc::command("set_muted", json!({"accountId": sel.account_id,
                                "conversationId": sel.conversation_id, "muted": event_target_checked(&ev)})) />
                        "Mute notifications (mentions still notify)"
                    </label>
                    <Show when=move || kind() != ConversationKind::Dm>
                        <button class="btn danger" on:click=leave>{icon(Icon::Leave)}
                            {move || if is_dm(kind()) { "Leave conversation" } else { "Leave channel" }}</button>
                    </Show>
                </div>
            </div>
        </aside>
    }
}

/// Notification preferences for one account. The server's notify module reads them.
#[component]
pub fn Prefs(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    let prefs = RwSignal::new(None::<Preferences>);
    let keyword = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let saved = RwSignal::new(false);
    leptos::task::spawn_local(async move {
        match ipc::invoke::<Preferences>("preferences", json!({"accountId": account_id})).await {
            Ok(p) => prefs.set(Some(p)),
            Err(e) => error.set(Some(e)),
        }
    });
    let host = c
        .account(account_id)
        .map(|a| ui::host(&a.server_url))
        .unwrap_or_default();
    let save = move |_| {
        let Some(p) = prefs.get_untracked() else {
            return;
        };
        leptos::task::spawn_local(async move {
            match ipc::invoke::<serde_json::Value>(
                "set_preferences",
                json!({"accountId": account_id, "preferences": p}),
            )
            .await
            {
                Ok(_) => {
                    saved.set(true);
                    c.overlay.set(Overlay::None);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let add_keyword = move || {
        let k = keyword.get_untracked().trim().to_string();
        if k.is_empty() {
            return;
        }
        prefs.update(|p| {
            if let Some(p) = p
                && !p.keywords.iter().any(|x| x.eq_ignore_ascii_case(&k))
            {
                p.keywords.push(k);
            }
        });
        keyword.set(String::new());
    };
    let level = move || prefs.with(|p| p.as_ref().map(|p| p.channel_level).unwrap_or_default());
    let set_level = move |l: NotifyLevel| {
        prefs.update(|p| {
            if let Some(p) = p {
                p.channel_level = l
            }
        })
    };
    let title_of = move |id: Uuid| {
        c.conversation(Selection {
            account_id,
            conversation_id: id,
        })
        .map(|(_, cv)| cv.title)
        .unwrap_or_else(|| "A conversation you left".into())
    };
    view! {
        <Dialog label=format!("Notifications on {host}")>
            <div class="dbody">
                <fieldset class="radios">
                    <legend>"Notify me about channel messages"</legend>
                    <label><input type="radio" name="level" prop:checked=move || level() == NotifyLevel::All
                        on:change=move |_| set_level(NotifyLevel::All) />"Every new message"</label>
                    <label><input type="radio" name="level" prop:checked=move || level() != NotifyLevel::All
                        on:change=move |_| set_level(NotifyLevel::Mentions) />"Mentions, keywords and replies in my threads"</label>
                    <p class="muted">"Direct messages always notify unless muted. Mentions notify even in muted channels."</p>
                </fieldset>
                <div class="sect">"Keywords"</div>
                <div class="chips">
                    {move || prefs.with(|p| p.as_ref().map(|p| p.keywords.clone()).unwrap_or_default()).into_iter().map(|k| {
                        let k2 = k.clone();
                        view! { <span class="chip on">{k.clone()}
                            <button class="iconbtn xs" aria-label=format!("Remove keyword {k}")
                                on:click=move |_| prefs.update(|p| if let Some(p) = p { p.keywords.retain(|x| *x != k2) })>
                                {sized(Icon::Close, 12)}</button></span> }
                    }).collect_view()}
                </div>
                <form class="inline" on:submit=move |ev| { ev.prevent_default(); add_keyword(); }>
                    <input class="field" aria-label="New keyword" placeholder="Add a keyword, like deploy"
                        prop:value=move || keyword.get() on:input=move |ev| keyword.set(event_target_value(&ev)) />
                    <button type="submit" class="btn">"Add"</button>
                </form>
                <div class="sect">"Muted conversations"</div>
                <ul class="mlist">
                    {move || {
                        let muted = prefs.with(|p| p.as_ref().map(|p| p.muted.clone()).unwrap_or_default());
                        if muted.is_empty() {
                            view! { <li class="muted">"Nothing is muted."</li> }.into_any()
                        } else {
                            muted.into_iter().map(|id| view! {
                                <li class="mrow"><span class="mname">{sized(Icon::BellOff, 14)}" "{title_of(id)}</span>
                                    <button class="plink" on:click=move |_| prefs.update(|p| if let Some(p) = p { p.muted.retain(|x| *x != id) })>"Unmute"</button></li>
                            }).collect_view().into_any()
                        }
                    }}
                </ul>
                {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
            </div>
            <footer class="dfoot">
                <button class="btn" on:click=move |_| c.overlay.set(Overlay::None)>"Cancel"</button>
                <button class="btn primary" on:click=save disabled=move || prefs.with(|p| p.is_none())>"Save"</button>
            </footer>
        </Dialog>
    }
}

/// A person's card: name, presence and a way to message them.
#[component]
pub fn Person(account_id: Uuid, user_id: Uuid) -> impl IntoView {
    let c = ctx();
    let place = c.place.get_untracked();
    let ws = place.and_then(|p| p.workspace_id);
    let member = RwSignal::new(None::<Member>);
    leptos::task::spawn_local(async move {
        if let Some(ws) = ws
            && let Ok(list) = ipc::invoke::<Vec<Member>>(
                "workspace_members",
                json!({"accountId": account_id, "workspaceId": ws}),
            )
            .await
        {
            member.set(list.into_iter().find(|m| m.user_id == user_id));
        }
    });
    let me = c.me(account_id) == Some(user_id);
    view! {
        <Dialog label="Profile">
            <div class="dbody person">
                {move || member.get().map(|m| view! {
                    {avatar(m.user_id, &m.display_name, "l")}
                    <div>
                        <div class="pname">{m.display_name.clone()}{me.then_some(" (you)")}</div>
                        <div class="pstat">{move || presence_dot(c.online(account_id, user_id))}
                            {move || if c.online(account_id, user_id) { "Active" } else { "Away" }}
                            " · "{format!("{:?}", m.role)}</div>
                    </div>
                })}
            </div>
            <footer class="dfoot">
                {(!me).then(|| view! {
                    <button class="btn primary" disabled=ws.is_none()
                        on:click=move |_| if let Some(ws) = ws { c.open_dm(account_id, ws, vec![user_id]) }>"Message"</button>
                })}
            </footer>
        </Dialog>
    }
}

/// New channel, browse channels, or a new direct message, in the current workspace.
#[component]
pub fn Tools(account_id: Uuid, workspace_id: Uuid, which: Overlay) -> impl IntoView {
    let c = ctx();
    let name = RwSignal::new(String::new());
    let private = RwSignal::new(false);
    let listed = RwSignal::new(Vec::<Conversation>::new());
    let members = RwSignal::new(Vec::<Member>::new());
    let chosen = RwSignal::new(Vec::<Uuid>::new());
    let filter = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let me = c.me(account_id).unwrap_or_default();
    let args = json!({"accountId": account_id, "workspaceId": workspace_id});
    match which {
        Overlay::Browse => leptos::task::spawn_local(async move {
            match ipc::invoke("browse_channels", args).await {
                Ok(l) => listed.set(l),
                Err(e) => error.set(Some(e)),
            }
        }),
        Overlay::NewDm => leptos::task::spawn_local(async move {
            match ipc::invoke("workspace_members", args).await {
                Ok(m) => members.set(m),
                Err(e) => error.set(Some(e)),
            }
        }),
        _ => {}
    }
    let create = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let args = json!({"accountId": account_id, "workspaceId": workspace_id,
            "name": name.get_untracked(), "private": private.get_untracked()});
        leptos::task::spawn_local(async move {
            match ipc::invoke::<Conversation>("create_channel", args).await {
                Ok(conv) => {
                    c.overlay.set(Overlay::None);
                    c.open_when_cached(account_id, conv.id);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let err = move || {
        error
            .get()
            .map(|e| view! { <p class="error" role="alert">{e}</p> })
    };
    match which {
        Overlay::NewChannel => view! {
            <Dialog label="Create a channel">
                <form on:submit=create>
                    <div class="dbody">
                        <label class="flabel">"Name"
                            <input class="field" required autofocus placeholder="e.g. release-planning"
                                prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) /></label>
                        <label class="chk"><input type="checkbox" prop:checked=move || private.get()
                            on:change=move |ev| private.set(event_target_checked(&ev)) />
                            "Private: only people you add can see it"</label>
                        {err}
                    </div>
                    <footer class="dfoot"><button type="submit" class="btn primary">"Create"</button></footer>
                </form>
            </Dialog>
        }.into_any(),
        Overlay::Browse => view! {
            <Dialog label="Browse channels">
                <div class="dbody">
                    {err}
                    <ul class="mlist">
                        {move || {
                            let l = listed.get();
                            if l.is_empty() { return view! { <li class="muted">"No channels yet."</li> }.into_any(); }
                            let mut l = l;
                            let joined = move |id: Uuid| c.conversation(Selection { account_id, conversation_id: id }).is_some();
                            // Channels to join first, then the ones already in the sidebar.
                            l.sort_by_key(|x| (joined(x.id), x.name.clone()));
                            l.into_iter().map(|conv| {
                                let id = conv.id;
                                let is_in = joined(id);
                                let glyph = if conv.kind == ConversationKind::Private { Icon::Lock } else { Icon::Hash };
                                view! { <li class="mrow"><span class="mname">{icon(glyph)}{conv.name.clone().unwrap_or_default()}</span>
                                    {if is_in {
                                        view! { <button class="btn" on:click=move |_| { c.overlay.set(Overlay::None); c.open(account_id, id); }>"Open"</button> }.into_any()
                                    } else {
                                        view! { <button class="btn primary" on:click=move |_| {
                                            let args = json!({"accountId": account_id, "conversationId": id});
                                            leptos::task::spawn_local(async move {
                                                match ipc::invoke::<serde_json::Value>("join_channel", args).await {
                                                    Ok(_) => { c.overlay.set(Overlay::None); c.open_when_cached(account_id, id); }
                                                    Err(e) => error.set(Some(e)),
                                                }
                                            });
                                        }>"Join"</button> }.into_any()
                                    }}</li> }
                            }).collect_view().into_any()
                        }}
                    </ul>
                </div>
            </Dialog>
        }.into_any(),
        _ => view! {
            <Dialog label="New message">
                <div class="dbody">
                    <div class="find">{sized(Icon::Search, 14)}
                        <input autofocus aria-label="Find people" placeholder="Find people"
                            prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) /></div>
                    <ul class="mlist">
                        {move || {
                            let f = filter.get().to_lowercase();
                            members.get().into_iter().filter(|m| m.user_id != me && m.display_name.to_lowercase().contains(&f)).map(|m| {
                                let uid = m.user_id;
                                view! { <li class="mrow"><label class="mperson">
                                    <input type="checkbox" prop:checked=move || chosen.with(|c| c.contains(&uid))
                                        on:change=move |ev| chosen.update(|c| if event_target_checked(&ev) { c.push(uid) } else { c.retain(|x| *x != uid) }) />
                                    {avatar(uid, &m.display_name, "m")}<span class="mname">{m.display_name.clone()}</span>
                                    {move || presence_dot(c.online(account_id, uid))}</label></li> }
                            }).collect_view()
                        }}
                    </ul>
                    {err}
                </div>
                <footer class="dfoot">
                    <span class="muted">{move || match chosen.with(|c| c.len()) { 0 => "Pick one person, or up to eight for a group".to_string(), n => format!("{n} selected") }}</span>
                    <button class="btn primary" disabled=move || chosen.with(|c| c.is_empty())
                        on:click=move |_| c.open_dm(account_id, workspace_id, chosen.get_untracked())>"Start"</button>
                </footer>
            </Dialog>
        }.into_any(),
    }
}

/// Signs an account whose session ended back in, keeping its cache.
#[component]
pub fn SignInAgain(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let a = c.account(account_id);
    let host = a
        .as_ref()
        .map(|a| ui::host(&a.server_url))
        .unwrap_or_default();
    let email = a.map(|a| a.email).unwrap_or_default();
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": account_id, "password": password.get_untracked()});
            match ipc::invoke::<serde_json::Value>("sign_in_again", args).await {
                Ok(_) => c.overlay.set(Overlay::None),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <Dialog label=format!("Sign in to {host}")>
            <form on:submit=submit>
                <div class="dbody">
                    <p class="muted">"Your session on this server ended. Your saved messages and anything waiting to send are kept."</p>
                    <label class="flabel">"Email"<input class="field" type="email" readonly prop:value=email /></label>
                    <label class="flabel">"Password"<input class="field" type="password" required autofocus autocomplete="current-password"
                        prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) /></label>
                    {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                </div>
                <footer class="dfoot"><button type="submit" class="btn primary" disabled=move || busy.get()>"Sign in"</button></footer>
            </form>
        </Dialog>
    }
}
