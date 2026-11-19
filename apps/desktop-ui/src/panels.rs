//! Side panels and dialogs: members, notification preferences, people, channel tools.
use leptos::prelude::*;
use proto::model::{Conversation, ConversationKind, Member, NotifyLevel, Preferences, Role};
use proto::view::{AccountView, ConnectionState, SessionEnd};
use serde_json::json;
use uuid::Uuid;
use wasm_bindgen::JsCast;

use crate::ipc;
use crate::state::{Aside, Overlay, Selection, ctx, is_dm};
use crate::ui::{self, Icon, avatar, icon, sized};

/// What Tab can reach inside a dialog.
const TABBABLE: &str = "a[href], button:not([disabled]), input:not([disabled]):not([type=hidden]), \
    select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])";

/// What a key does while a dialog is open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DialogKey {
    Close,
    /// Focus the tab stop at this index.
    Focus(usize),
    /// Keep focus where it is: there is nothing to Tab to.
    Hold,
    /// Leave the key to the browser and the focused control.
    Pass,
}

/// Escape closes from anywhere unless a control used it. Tab wraps around the dialog's `stops`;
/// `at` is the focused stop, None when focus is on the dialog itself or outside it.
pub(crate) fn dialog_key(
    key: &str,
    shift: bool,
    used: bool,
    stops: usize,
    at: Option<usize>,
) -> DialogKey {
    match key {
        "Escape" if !used => DialogKey::Close,
        "Tab" if stops == 0 => DialogKey::Hold,
        "Tab" => match (at, shift) {
            (None, false) => DialogKey::Focus(0),
            (None, true) | (Some(0), true) => DialogKey::Focus(stops - 1),
            (Some(i), false) if i + 1 >= stops => DialogKey::Focus(0),
            _ => DialogKey::Pass,
        },
        _ => DialogKey::Pass,
    }
}

fn tab_stops(root: &web_sys::Element) -> Vec<web_sys::HtmlElement> {
    let Ok(list) = root.query_selector_all(TABBABLE) else {
        return vec![];
    };
    (0..list.length())
        .filter_map(|i| list.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        .filter(|e| e.get_client_rects().length() > 0)
        .collect()
}

/// Puts focus inside `root` unless it is already there: on a field marked `autofocus`, or
/// else on the dialog itself, so a Return can't trigger its first button by accident.
fn focus_into(root: &web_sys::HtmlElement) {
    let active = document().active_element();
    if active.is_some_and(|a| root.contains(Some(a.as_ref()))) {
        return;
    }
    match root
        .query_selector("[autofocus]")
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        Some(e) => {
            let _ = e.focus();
        }
        None => {
            let _ = root.focus();
        }
    }
}

/// A modal dialog over a scrim. It takes focus when it opens and keeps Tab inside; Escape
/// or a click on the scrim closes it, and focus goes back to where it was.
#[component]
pub fn Dialog(#[prop(into)] label: String, children: Children) -> impl IntoView {
    let c = ctx();
    let aria = label.clone();
    let section: NodeRef<leptos::html::Section> = NodeRef::new();
    let opener = StoredValue::new_local(document().active_element());
    Effect::new(move |_| {
        if let Some(s) = section.get() {
            // After the children's own focus effects, which win.
            ipc::next_frame(move || focus_into(&s));
        }
    });
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        let Some(s) = section.get_untracked() else {
            return;
        };
        let stops = tab_stops(&s);
        let active = document().active_element();
        let at = active
            .as_ref()
            .and_then(|a| stops.iter().position(|e| e.is_same_node(Some(a.as_ref()))));
        match dialog_key(
            &ev.key(),
            ev.shift_key(),
            ev.default_prevented(),
            stops.len(),
            at,
        ) {
            DialogKey::Close => {
                ev.prevent_default();
                c.overlay.set(Overlay::None);
            }
            DialogKey::Focus(i) => {
                ev.prevent_default();
                let _ = stops[i].focus();
            }
            DialogKey::Hold => ev.prevent_default(),
            DialogKey::Pass => {}
        }
    });
    // Focus moved out from code, such as a composer focusing itself: bring it back.
    let held = window_event_listener(leptos::ev::focusin, move |ev| {
        if let Some(s) = section.get_untracked()
            && let Some(t) = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok())
            && !s.contains(Some(&t))
        {
            // Not from inside this handler: the focus change would call it again.
            queue_microtask(move || focus_into(&s));
        }
    });
    on_cleanup(move || {
        keys.remove();
        held.remove();
        ui::give_back_focus(
            opener.try_update_value(|o| o.take()).flatten(),
            section.get_untracked().map(Into::into),
        );
    });
    view! {
        <div class="scrim" on:click=move |_| c.overlay.set(Overlay::None)></div>
        <section class="pop dlg" role="dialog" aria-modal="true" aria-label=aria tabindex="-1" node_ref=section>
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

/// What the signed-out sign-in UI shows for an account; None while it is signed in.
#[derive(Clone, PartialEq)]
struct SignedOut {
    host: String,
    reason: String,
}

fn signed_out(a: &AccountView) -> Option<SignedOut> {
    if a.connection != ConnectionState::SignedOut {
        return None;
    }
    let host = ui::host(&a.server_url);
    let reason = match a.session_end {
        Some(SessionEnd::NoToken) => format!("This Mac has no saved sign-in for {}.", a.email),
        _ => format!("{host} ended this Mac's session, for example after a password reset."),
    };
    Some(SignedOut { host, reason })
}

/// Messages in the account's outbox.
fn waiting(a: &AccountView) -> u32 {
    a.workspaces
        .iter()
        .flat_map(|w| &w.conversations)
        .map(|cv| cv.pending)
        .sum()
}

fn messages(n: u32) -> String {
    format!("{n} {}", if n == 1 { "message" } else { "messages" })
}

/// Signs the account out and removes it from this Mac, with a notice if the server could
/// not be told.
pub fn remove_account(account_id: Uuid, host: String) {
    let c = ctx();
    leptos::task::spawn_local(async move {
        match ipc::invoke::<bool>("remove_account", json!({"accountId": account_id})).await {
            Ok(true) => {}
            Ok(false) => c.notice.set(Some(format!(
                "Removed from this Mac, but {host} could not be reached to end the session. It may still be active on the server."))),
            Err(e) => ipc::warn(format!("remove_account: {e}")),
        }
    });
}

/// Sends the password; on failure, the error is worded for the person signing in.
fn sign_in(
    account_id: Uuid,
    password: RwSignal<String>,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    done: impl Fn() + 'static,
) {
    if busy.get_untracked() {
        return;
    }
    busy.set(true);
    error.set(None);
    leptos::task::spawn_local(async move {
        let args = json!({"accountId": account_id, "password": password.get_untracked()});
        match ipc::invoke::<serde_json::Value>("sign_in_again", args).await {
            Ok(_) => {
                password.set(String::new());
                done();
            }
            Err(e) => error.set(Some(e)),
        }
        busy.set(false);
    });
}

/// Signs the account back in through the server's provider, in the browser.
fn oidc_sign_in_again(
    account_id: Uuid,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    done: impl Fn() + 'static,
) {
    if busy.get_untracked() {
        return;
    }
    busy.set(true);
    error.set(None);
    leptos::task::spawn_local(async move {
        let args = json!({"accountId": account_id});
        match ipc::invoke::<serde_json::Value>("oidc_sign_in_again", args).await {
            Ok(_) => done(),
            Err(e) => error.set(Some(e)),
        }
        busy.set(false);
    });
}

/// Focuses the element each time it is mounted; `autofocus` only applies on page load.
fn focus_when_mounted<E>(node: NodeRef<E>)
where
    E: leptos::html::ElementType,
    E::Output: wasm_bindgen::JsCast + Clone + 'static,
{
    Effect::new(move |_| {
        if let Some(el) = node.get() {
            let _ = wasm_bindgen::JsCast::unchecked_ref::<web_sys::HtmlElement>(&el).focus();
        }
    });
}

/// The id of the signed-out card's password field, which the header's "Signed out" button focuses.
pub const SESSION_PASSWORD: &str = "session-password";

/// The sign-in card docked above the composer while the account is signed out.
#[component]
pub fn SessionCard(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let confirm = RwSignal::new(false);
    let cancel = NodeRef::<leptos::html::Button>::new();
    focus_when_mounted(cancel);
    let info = Memo::new(move |_| c.account(account_id).as_ref().and_then(signed_out));
    let pending = Memo::new(move |_| c.account(account_id).map(|a| waiting(&a)).unwrap_or(0));
    let provider = crate::settings::provider_of(
        c.account(account_id)
            .map(|a| a.server_url)
            .unwrap_or_default(),
    );
    let oidc_busy = RwSignal::new(false);
    let start_oidc = move || oidc_sign_in_again(account_id, oidc_busy, error, || {});
    move || {
        let s = info.get()?;
        let host = s.host.clone();
        Some(if confirm.get() {
            let gone = host.clone();
            view! {
                <section class="sess" aria-labelledby="sess-title">
                    <span class="lockchip danger">{sized(Icon::Trash, 17)}</span>
                    <div class="sess-b">
                        <div>
                            <h2 class="sess-t" id="sess-title">{format!("Remove {host} from this Mac?")}</h2>
                            <p class="sess-d">{move || match pending.get() {
                                0 => "This deletes its saved messages here.".to_string(),
                                n => format!("This deletes its saved messages here, and {} that {} been sent.",
                                    messages(n), if n == 1 { "hasn't" } else { "haven't" }),
                            }}{format!(" Your account on {host} isn't affected.")}</p>
                        </div>
                        <div class="sess-f">
                            <button class="btn danger" on:click=move |_| remove_account(account_id, gone.clone())>"Remove"</button>
                            <button class="btn" node_ref=cancel on:click=move |_| confirm.set(false)>"Cancel"</button>
                        </div>
                    </div>
                </section>
            }.into_any()
        } else {
            view! {
                <section class="sess" aria-labelledby="sess-title" aria-busy=move || busy.get().to_string()>
                    <span class="lockchip">{sized(Icon::Lock, 17)}</span>
                    <div class="sess-b">
                        <div>
                            <h2 class="sess-t" id="sess-title">{format!("You're signed out of {host}")}</h2>
                            <p class="sess-d">{s.reason.clone()}" Sign in again to get new messages and send."</p>
                        </div>
                        <div class="can">
                            <span>{sized(Icon::Check, 13)}"Messages saved on this Mac stay readable"</span>
                            {move || (pending.get() > 0).then(|| view! {
                                <span>{sized(Icon::Clock, 13)}{format!("{} {} in your outbox", messages(pending.get()),
                                    if pending.get() == 1 { "waits" } else { "wait" })}</span>
                            })}
                        </div>
                        {move || provider.get().map(|p| view! {
                            <div class="sess-f">
                                <crate::settings::ProviderButton name=p.clone() busy=oidc_busy label=format!("Continue with {p}") on_start=start_oidc />
                            </div>
                        })}
                        <form class="sess-f" on:submit=move |ev| {
                            ev.prevent_default();
                            sign_in(account_id, password, busy, error, || {});
                        }>
                            <label class="flab">"Password"
                                <input class="field" id=SESSION_PASSWORD type="password" required autocomplete="current-password"
                                    readonly=move || busy.get()
                                    class:bad=move || error.with(|e| e.is_some())
                                    aria-invalid=move || error.with(|e| e.is_some()).to_string()
                                    aria-describedby="sess-err"
                                    prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) /></label>
                            <button type="submit" class="btn primary" disabled=move || busy.get()>
                                {move || if busy.get() { "Signing in…" } else { "Sign in" }}</button>
                            <button type="button" class="qbtn" on:click=move |_| confirm.set(true)>"Remove from this Mac…"</button>
                        </form>
                        {move || error.get().map(|e| view! {
                            <p class="serr" id="sess-err" role="alert">{sized(Icon::Info, 14)}{e}</p>
                        })}
                    </div>
                </section>
            }.into_any()
        })
    }
}

/// The sign-in sheet for a signed-out account, opened from the switcher.
#[component]
pub fn SignInAgain(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let confirm = RwSignal::new(false);
    let password_field = NodeRef::<leptos::html::Input>::new();
    let cancel = NodeRef::<leptos::html::Button>::new();
    focus_when_mounted(password_field);
    focus_when_mounted(cancel);
    let a = c.account(account_id);
    let s = a.as_ref().and_then(signed_out);
    let pending = a.as_ref().map(waiting).unwrap_or(0);
    let host = a
        .as_ref()
        .map(|a| ui::host(&a.server_url))
        .unwrap_or_default();
    let email = a.map(|a| a.email).unwrap_or_default();
    let reason = s.map(|s| s.reason).unwrap_or_default();
    let kept = match pending {
        0 => "Saved messages and drafts stay on this Mac.".to_string(),
        n => format!(
            "Saved messages, drafts and {} waiting to send stay on this Mac.",
            messages(n)
        ),
    };
    let gone = host.clone();
    let removing = match pending {
        0 => {
            format!("This deletes its saved messages here. Your account on {host} isn't affected.")
        }
        n => format!(
            "This deletes its saved messages here, and {} that {} been sent. Your account on {host} isn't affected.",
            messages(n),
            if n == 1 { "hasn't" } else { "haven't" }
        ),
    };
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        sign_in(account_id, password, busy, error, move || {
            c.overlay.set(Overlay::None)
        });
    };
    let provider = crate::settings::provider_of(
        c.account(account_id)
            .map(|a| a.server_url)
            .unwrap_or_default(),
    );
    let oidc_busy = RwSignal::new(false);
    let start_oidc = move || {
        oidc_sign_in_again(account_id, oidc_busy, error, move || {
            c.overlay.set(Overlay::None)
        })
    };
    view! {
        <Dialog label=format!("Sign in to {host}")>
            <form on:submit=submit>
                <div class="dbody">
                    <p class="muted" style="margin: 0">{reason}</p>
                    {move || provider.get().map(|p| view! {
                        <crate::settings::ProviderButton name=p.clone() busy=oidc_busy label=format!("Continue with {p}") on_start=start_oidc />
                        <div class="divider" role="separator"><span>"or with the password"</span></div>
                    })}
                    <label class="flabel">"Email"<input class="field" type="email" readonly autocomplete="username" prop:value=email /></label>
                    <label class="flabel">"Password"<input class="field" type="password" required autocomplete="current-password"
                        readonly=move || busy.get() node_ref=password_field
                        aria-invalid=move || error.with(|e| e.is_some()).to_string()
                        prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) /></label>
                    {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                    <div class="kept">{sized(Icon::Check, 14)}<span>{kept}</span></div>
                </div>
                {move || if confirm.get() {
                    let gone = gone.clone();
                    view! {
                        <footer class="dfoot" role="group" aria-label="Remove from this Mac?">
                            <span class="muted">{removing.clone()}</span>
                            <button type="button" class="btn" node_ref=cancel on:click=move |_| confirm.set(false)>"Cancel"</button>
                            <button type="button" class="btn danger" on:click=move |_| {
                                c.overlay.set(Overlay::None);
                                remove_account(account_id, gone.clone());
                            }>"Remove"</button>
                        </footer>
                    }.into_any()
                } else {
                    view! {
                        <footer class="dfoot">
                            <button type="button" class="btn danger" style="margin-right: auto" on:click=move |_| confirm.set(true)>"Remove from this Mac…"</button>
                            <button type="button" class="btn" on:click=move |_| c.overlay.set(Overlay::None)>"Cancel"</button>
                            <button type="submit" class="btn primary" disabled=move || busy.get()>
                                {move || if busy.get() { "Signing in…" } else { "Sign in" }}</button>
                        </footer>
                    }.into_any()
                }}
            </form>
        </Dialog>
    }
}

/// Creates a single-use invite link for one email address and copies it (owners and admins).
#[component]
pub fn InvitePeople(account_id: Uuid, workspace_id: Uuid) -> impl IntoView {
    let c = ctx();
    let email = RwSignal::new(String::new());
    let role = RwSignal::new("member".to_string());
    let days = RwSignal::new(7u32);
    let made = RwSignal::new(None::<(String, bool, String, String)>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let name = c
        .workspace(account_id, workspace_id)
        .map(|w| w.name)
        .unwrap_or_default();
    let create = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let to = email.get_untracked().trim().to_string();
        if !looks_like_email(&to) {
            error.set(Some(
                "Enter the email address of the person you're inviting.".into(),
            ));
            return;
        }
        busy.set(true);
        error.set(None);
        let args = json!({"accountId": account_id, "workspaceId": workspace_id, "email": to,
                          "role": role.get_untracked(), "expiresInDays": days.get_untracked()});
        leptos::task::spawn_local(async move {
            match ipc::invoke::<proto::api::InviteCreated>("create_invite", args).await {
                Ok(i) => {
                    let copied = crate::ui::copy_text(&i.link).await;
                    let until = js_sys::Date::new(&(i.invite.expires_at_ms as f64).into())
                        .to_locale_date_string("default", &js_sys::Object::new())
                        .as_string()
                        .unwrap_or_default();
                    let to = i.invite.email.unwrap_or_default();
                    made.set(Some((i.link, copied, to, until)));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <Dialog label=format!("Invite people to {name}")>
            <form on:submit=create>
                <div class="dbody">
                    <p class="muted">"Each link works once, for the email address you enter. Send it to that person; they paste it into Commons and sign up with that address."</p>
                    <label class="flabel">"Email"
                        <input class="field" type="email" required autofocus autocomplete="off" placeholder="name@example.org"
                            prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev)) /></label>
                    <label class="flabel">"Joins as"
                        <select class="field" on:change=move |ev| role.set(event_target_value(&ev))>
                            <option value="member" selected=true>"Member"</option>
                            <option value="guest">"Guest: only the channels they're added to"</option>
                        </select></label>
                    <label class="flabel">"Expires after"
                        <select class="field" on:change=move |ev| days.set(event_target_value(&ev).parse().unwrap_or(7))>
                            <option value="1">"1 day"</option>
                            <option value="7" selected=true>"7 days"</option>
                            <option value="30">"30 days"</option>
                        </select></label>
                    {move || made.get().map(|(link, copied, to, until)| view! {
                        <label class="flabel">{if copied { "Copied to the clipboard" } else { "Copy this link" }}
                            <input class="field" readonly prop:value=link on:focus=move |ev| {
                                if let Some(i) = ev.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlInputElement>(t).ok()) { i.select() }
                            } /></label>
                        <p class="muted">{format!("Invite for {to} · expires {until}")}</p>
                    })}
                    {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                </div>
                <footer class="dfoot"><button type="submit" class="btn primary" disabled=move || busy.get()>
                    {move || if made.get().is_some() { "Create another link" } else { "Create link" }}</button></footer>
            </form>
        </Dialog>
    }
}

/// A light check before the server's: something on each side of one `@`.
pub(crate) fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    matches!(s.split_once('@'), Some((user, host)) if !user.is_empty() && !host.is_empty()
        && !host.contains('@') && !s.contains(char::is_whitespace))
}

/// Joins another workspace with a pasted invite link, as the account on that link's server.
/// What an opened invite offers, under its server's host. `new_server` adds the warning for
/// a server this app has no account on.
#[component]
pub fn InviteSummary(invite: proto::view::OpenedInvite, new_server: bool) -> impl IntoView {
    let p = invite.preview;
    let host = invite.host;
    let notice_host = host.clone();
    view! {
        <div class="invsum">
            <p class="invhost">"Join "<b>{host}</b></p>
            <p>{p.inviter_name}" invited "<b>{p.email_masked}</b>" to "<b>{p.workspace_name}</b>
                {format!(" as a {}.", p.role.as_str())}</p>
            <p class="muted">{format!("The invite expires {}.", crate::settings::date(p.expires_at_ms))}</p>
        </div>
        {new_server.then(|| view! {
            <div class="newserver" role="note">
                <b>"New server. "</b>
                {format!("This app has no account on {notice_host}. Continue only if you expected an invite to it.")}
            </div>
        })}
    }
}

/// Joins a workspace with an invite link, as an account on its server. Opened from a
/// `commons://` link it comes filled in, and asks which account when there are several.
#[component]
pub fn JoinInvite() -> impl IntoView {
    let c = ctx();
    let pasted = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let opened = c.opened_invite;
    let chosen = RwSignal::new(None::<Uuid>);
    Effect::new(move |_| {
        chosen.set(opened.with(|o| o.as_ref().and_then(|o| o.accounts.first().copied())));
        error.set(None);
    });
    let close = move || {
        opened.set(None);
        c.overlay.set(Overlay::None);
    };
    let join = move |account_id: Uuid, token: String| {
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": account_id, "token": token});
            match ipc::invoke::<proto::model::Workspace>("accept_invite", args).await {
                Ok(w) => {
                    close();
                    c.go_to_workspace(account_id, Some(w.id));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if let Some(o) = opened.get_untracked() {
            if let Some(account_id) = chosen.get_untracked() {
                join(account_id, o.token);
            }
            return;
        }
        let link = pasted.get_untracked();
        let (server, token) = proto::api::split_invite(&link);
        let accounts = c.sidebar.get_untracked().accounts;
        let same = |a: &&proto::view::AccountView| {
            server.as_deref().map(|s| s.trim_end_matches('/'))
                == Some(a.server_url.trim_end_matches('/'))
        };
        let account = match (&server, accounts.len()) {
            (Some(_), _) => accounts.iter().find(same).map(|a| a.id),
            (None, 1) => accounts.first().map(|a| a.id),
            (None, _) => {
                error.set(Some(
                    "Paste the whole link, so Commons knows which server it's for.".into(),
                ));
                return;
            }
        };
        let Some(account_id) = account else {
            // No account on that server yet: sign up or in there, with the link.
            c.invite_link.set(Some(link));
            c.overlay.set(Overlay::None);
            c.adding.set(true);
            return;
        };
        join(account_id, token);
    };
    let email_of = move |id: Uuid| c.account(id).map(|a| a.email).unwrap_or_default();
    view! {
        <Dialog label="Join with an invite link">
            <form on:submit=submit>
                <div class="dbody">
                    {move || match opened.get() {
                        Some(o) => {
                            let accounts = o.accounts.clone();
                            let several = accounts.len() > 1;
                            view! {
                                <InviteSummary invite=o new_server=false />
                                {several.then(|| view! {
                                    <fieldset class="pick">
                                        <legend>"Join as"</legend>
                                        {accounts.into_iter().map(|id| view! {
                                            <label class="chk"><input type="radio" name="joinas"
                                                prop:checked=move || chosen.get() == Some(id)
                                                on:change=move |_| chosen.set(Some(id)) />{email_of(id)}</label>
                                        }).collect_view()}
                                    </fieldset>
                                })}
                                {(!several).then(|| view! {
                                    <p class="muted">{move || chosen.get().map(|id| format!("Joining as {}", email_of(id)))}</p>
                                })}
                            }.into_any()
                        }
                        None => view! {
                            <label class="flabel">"Invite link"
                                <input class="field" required autofocus placeholder="https://…/invite#…"
                                    prop:value=move || pasted.get() on:input=move |ev| pasted.set(event_target_value(&ev)) /></label>
                        }.into_any(),
                    }}
                    {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
                </div>
                <footer class="dfoot">
                    <Show when=move || opened.with(|o| o.is_some())>
                        <button type="button" class="btn" on:click=move |_| close()>"Not now"</button>
                        // The invite may be for an address none of these accounts has.
                        <button type="button" class="btn" on:click=move |_| {
                            c.overlay.set(Overlay::None);
                            c.adding.set(true);
                        }>"Use another account"</button>
                    </Show>
                    <button type="submit" class="btn primary" disabled=move || busy.get()>"Join"</button>
                </footer>
            </form>
        </Dialog>
    }
}

#[cfg(test)]
mod tests {
    use super::{DialogKey, dialog_key, looks_like_email};

    #[test]
    fn escape_closes_a_dialog_from_anywhere_unless_a_control_used_it() {
        // Focus inside, on the dialog itself, or left behind in a composer: all close.
        for at in [Some(1), None] {
            assert_eq!(dialog_key("Escape", false, false, 3, at), DialogKey::Close);
        }
        assert_eq!(
            dialog_key("Escape", false, true, 3, Some(0)),
            DialogKey::Pass
        );
        assert_eq!(dialog_key("Enter", false, false, 3, None), DialogKey::Pass);
    }

    #[test]
    fn tab_wraps_inside_a_dialog() {
        assert_eq!(
            dialog_key("Tab", false, false, 3, Some(2)),
            DialogKey::Focus(0)
        );
        assert_eq!(
            dialog_key("Tab", true, false, 3, Some(0)),
            DialogKey::Focus(2)
        );
        // From the dialog itself or from behind it, Tab enters at the edge.
        assert_eq!(
            dialog_key("Tab", false, false, 3, None),
            DialogKey::Focus(0)
        );
        assert_eq!(dialog_key("Tab", true, false, 3, None), DialogKey::Focus(2));
        // In between, the browser moves focus as usual.
        assert_eq!(dialog_key("Tab", false, false, 3, Some(0)), DialogKey::Pass);
        assert_eq!(dialog_key("Tab", true, false, 3, Some(2)), DialogKey::Pass);
        assert_eq!(dialog_key("Tab", false, false, 0, None), DialogKey::Hold);
    }

    #[test]
    fn an_invite_needs_something_like_an_email() {
        assert!(looks_like_email(" x@gmail.com "));
        for bad in ["", "x", "x@", "@gmail.com", "a@b@c", "a b@c.org"] {
            assert!(!looks_like_email(bad), "{bad:?}");
        }
    }
}
