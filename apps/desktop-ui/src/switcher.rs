//! The accounts and workspaces switcher, opened from the current workspace.
use leptos::prelude::*;
use proto::view::{AccountView, ConnectionState, SessionEnd};
use serde_json::json;

use crate::ipc;
use crate::panels::ConfirmRemove;
use crate::state::{Overlay, ctx};
use crate::ui::{self, Icon, icon, sized};

pub fn status_label(a: &AccountView) -> (&'static str, &'static str) {
    match a.connection {
        ConnectionState::Online => ("Connected", "online"),
        ConnectionState::Connecting => ("Connecting", "busy"),
        ConnectionState::Offline => ("Reconnecting", "off"),
        ConnectionState::SignedOut => ("Signed out", "out"),
    }
}

#[component]
pub fn Switcher() -> impl IntoView {
    let c = ctx();
    let find = RwSignal::new(String::new());
    let manage = RwSignal::new(false);
    let removing = RwSignal::new(None::<uuid::Uuid>);
    let new_ws = RwSignal::new(None::<uuid::Uuid>);
    let ws_name = RwSignal::new(String::new());
    let close = move || c.overlay.set(Overlay::None);
    let order = move || c.workspace_order();

    let account = move |a: AccountView| {
        let account_id = a.id;
        let (status, dot) = status_label(&a);
        let signed_out = a.connection == ConnectionState::SignedOut;
        let ended = match a.session_end {
            Some(SessionEnd::NoToken) => "No saved sign-in on this Mac.",
            _ => "The server ended this Mac's session.",
        };
        let f = find.get().to_lowercase();
        let rows = a
            .workspaces
            .iter()
            .filter(|w| w.name.to_lowercase().contains(&f))
            .map(|w| {
                let wid = w.id;
                let current = move || c.place.get().is_some_and(|p| p.account_id == account_id && p.workspace_id == Some(wid));
                let unread: u32 = w.conversations.iter().filter(|cv| !cv.muted).map(|cv| cv.unread).sum();
                let mentions: u32 = w.conversations.iter().map(|cv| cv.mentions).sum();
                let n = order().iter().position(|x| *x == (account_id, wid));
                let kb = n.filter(|i| *i < 9).map(|i| format!("⌘{}", i + 1)).unwrap_or_default();
                view! {
                    <button class="wrow" class:cur=current aria-current=move || current().to_string()
                        on:click=move |_| { close(); c.go_to_workspace(account_id, Some(wid)); }>
                        <span class="ws sm" style=format!("background: {}", ui::color(wid))>{ui::tile(&w.name)}</span>
                        <span class="wn">{w.name.clone()}{signed_out.then(|| view! { <span class="wsub">" · saved copy"</span> })}</span>
                        {move || current().then(|| view! { <span class="cur-ic" role="img" aria-label="Current workspace">{icon(Icon::Check)}</span> })}
                        {if mentions > 0 {
                            Some(view! { <span class="cnt" aria-label=format!("{mentions} mentions")>{mentions}</span> }.into_any())
                        } else if unread > 0 {
                            Some(view! { <span class="udot" role="img" aria-label="Unread messages"></span> }.into_any())
                        } else { None }}
                        <span class="kb">{kb}</span>
                    </button>
                }
            })
            .collect_view();
        let host = ui::host(&a.server_url);
        view! {
            <div class="acct">
                <div class="ah">
                    <span class="ahs">{icon(Icon::Server)}</span>
                    <div>
                        <div class="adom">{host}</div>
                        <div class="aid">{format!("{} · email and password", a.display_name)}</div>
                    </div>
                    <span class="stat">{if signed_out { sized(Icon::Lock, 11).into_any() } else {
                        view! { <span class=format!("dot {dot}")></span> }.into_any() }}{status}</span>
                </div>
                {signed_out.then(|| view! {
                    <div class="signin">
                        <span style="flex: 1">{ended}" You can still read what's saved here; sending waits until you sign in."</span>
                        <button class="sbtn" on:click=move |_| c.overlay.set(Overlay::SignInAgain(account_id))>"Sign in…"</button>
                    </div>
                })}
                {rows}
                {move || if new_ws.get() == Some(account_id) {
                    view! {
                        <form class="inline" on:submit=move |ev| {
                            ev.prevent_default();
                            let name = ws_name.get_untracked();
                            if !name.trim().is_empty() {
                                ws_name.set(String::new());
                                new_ws.set(None);
                                ipc::command("create_workspace", json!({"accountId": account_id, "name": name}));
                            }
                        }>
                            <input class="field" autofocus aria-label="New workspace name" placeholder="Workspace name"
                                prop:value=move || ws_name.get() on:input=move |ev| ws_name.set(event_target_value(&ev)) />
                            <button type="submit" class="btn primary">"Create"</button>
                        </form>
                    }.into_any()
                } else if !signed_out {
                    view! { <button class="wrow add" on:click=move |_| new_ws.set(Some(account_id))>
                        <span class="ws sm ghost">{sized(Icon::Plus, 14)}</span><span class="wn">"New workspace"</span></button> }.into_any()
                } else { ().into_any() }}
                <Show when=move || manage.get()>
                    {move || if removing.get() == Some(account_id) {
                        view! {
                            <ConfirmRemove account_id=account_id class="manage removing"
                                on_cancel=Callback::new(move |_| removing.set(None)) />
                        }.into_any()
                    } else {
                        view! {
                            <div class="manage">
                                <button class="btn danger" on:click=move |_| removing.set(Some(account_id))>
                                    "Sign out and remove from this Mac…"</button>
                            </div>
                        }.into_any()
                    }}
                </Show>
            </div>
        }
    };

    view! {
        <div class="scrim" on:click=move |_| close()></div>
        <section class="pop switcher" role="dialog" aria-modal="true" aria-label="Switch workspace"
            on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Escape" { ev.prevent_default(); close() }>
            <label class="find">{sized(Icon::Search, 14)}
                <input type="text" autofocus placeholder="Find a workspace" aria-label="Find a workspace"
                    prop:value=move || find.get() on:input=move |ev| find.set(event_target_value(&ev)) /></label>
            <div class="accts">
                {move || c.sidebar.get().accounts.into_iter().map(account).collect_view()}
            </div>
            <div class="pfoot">
                <button class="fbtn" on:click=move |_| { close(); c.adding.set(true); }>{icon(Icon::Plus)}"Add a server account"</button>
                <button class="fbtn" on:click=move |_| { c.opened_invite.set(None); c.overlay.set(Overlay::JoinInvite) }>{icon(Icon::Link)}"Join with an invite link"</button>
                <button class="fbtn quiet" aria-pressed=move || manage.get().to_string()
                    on:click=move |_| manage.update(|m| *m = !*m)>
                    {move || if manage.get() { "Done" } else { "Manage accounts" }}</button>
            </div>
        </section>
    }
}
