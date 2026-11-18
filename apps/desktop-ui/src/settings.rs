//! Sign-in through the server's OIDC provider, account settings (link and unlink), and the
//! workspace admin screen for owners and admins.
use leptos::prelude::*;
use proto::api::{AdminMember, IdentityList, Invite, Meta};
use proto::model::Role;
use serde_json::json;
use uuid::Uuid;

use crate::ipc;
use crate::panels::Dialog;
use crate::state::{Overlay, ctx};
use crate::ui::{self, Icon, avatar, icon, sized};

/// The provider the server at `url` offers, if any; None until the server answers.
pub fn provider_of(url: String) -> RwSignal<Option<String>> {
    let name = RwSignal::new(None::<String>);
    leptos::task::spawn_local(async move {
        if let Ok(m) = ipc::invoke::<Meta>("server_meta", json!({"serverUrl": url})).await {
            name.set(m.oidc.map(|o| o.name));
        }
    });
    name
}

/// "Continue with Google" and, while the browser is open, a note and Cancel.
#[component]
pub fn ProviderButton(
    name: String,
    busy: RwSignal<bool>,
    #[prop(into)] label: String,
    on_start: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let waiting = format!("Finish in your browser. Commons is waiting for {name}…");
    view! {
        <Show when=move || busy.get() fallback=move || {
            let label = label.clone();
            view! {
                <button type="button" class="btn wide oidc" on:click=move |_| on_start()>
                    {sized(Icon::Link, 14)}{label}</button>
            }
        }>
            <div class="oidcwait" role="status">
                <span class="muted">{waiting.clone()}</span>
                <button type="button" class="btn" on:click=move |_| ipc::command("oidc_cancel", json!({}))>"Cancel"</button>
            </div>
        </Show>
    }
}

/// The date part of a millisecond timestamp, in the person's locale.
pub fn date(ms: i64) -> String {
    js_sys::Date::new(&(ms as f64).into())
        .to_locale_date_string("default", &js_sys::Object::new())
        .as_string()
        .unwrap_or_default()
}

/// Account settings: the ways this account signs in, with link and unlink.
#[component]
pub fn AccountSettings(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    let a = c.account(account_id);
    let host = a
        .as_ref()
        .map(|a| ui::host(&a.server_url))
        .unwrap_or_default();
    let email = a.as_ref().map(|a| a.email.clone()).unwrap_or_default();
    let name = a
        .as_ref()
        .map(|a| a.display_name.clone())
        .unwrap_or_default();
    let provider = provider_of(a.map(|a| a.server_url).unwrap_or_default());
    let list = RwSignal::new(None::<IdentityList>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let load = move || {
        leptos::task::spawn_local(async move {
            match ipc::invoke::<IdentityList>("identities", json!({"accountId": account_id})).await
            {
                Ok(l) => list.set(Some(l)),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    load();
    let link = move || {
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            match ipc::invoke::<IdentityList>("oidc_link", json!({"accountId": account_id})).await {
                Ok(l) => list.set(Some(l)),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let unlink = move |identity_id: Uuid| {
        error.set(None);
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": account_id, "identityId": identity_id});
            match ipc::invoke::<serde_json::Value>("unlink_identity", args).await {
                Ok(_) => load(),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let label = move || provider.get().unwrap_or_else(|| "Single sign-on".into());
    view! {
        <Dialog label=format!("Settings for {host}")>
            <div class="dbody">
                <div class="sect">"Account"</div>
                <label class="flabel">"Name"<input class="field" readonly prop:value=name /></label>
                <label class="flabel">"Email"<input class="field" readonly prop:value=email /></label>
                <div class="sect">"Ways to sign in"</div>
                <ul class="mlist">
                    {move || list.get().map(|l| {
                        let password = if l.has_password { "Set" } else { "Not set" };
                        let rows = l.identities.into_iter().map(|i| {
                            let who = i.email.clone().unwrap_or_else(|| ui::host(&i.issuer));
                            let aria = format!("Unlink {} ({who})", label());
                            view! {
                                <li class="mrow">
                                    <span class="mname">{sized(Icon::Link, 14)}{label()}</span>
                                    <span class="muted">{who}</span>
                                    <button class="plink" aria-label=aria on:click=move |_| unlink(i.id)>
                                        {format!("Unlink {}", label())}</button>
                                </li>
                            }
                        }).collect_view();
                        view! {
                            <li class="mrow">
                                <span class="mname">{sized(Icon::Lock, 14)}"Password"</span>
                                <span class="muted">{password}</span>
                            </li>
                            {rows}
                        }
                    })}
                </ul>
                {move || provider.get().map(|name| {
                    let linked = list.with(|l| l.as_ref().is_some_and(|l| !l.identities.is_empty()));
                    let label = if linked { format!("Link another {name} account") } else { format!("Link {name}") };
                    view! { <ProviderButton name=name.clone() busy=busy label=label on_start=link /> }
                })}
                {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
            </div>
            <footer class="dfoot">
                <button class="btn" on:click=move |_| c.overlay.set(Overlay::None)>"Close"</button>
            </footer>
        </Dialog>
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Owner => "Owner",
        Role::Admin => "Admin",
        Role::Member => "Member",
        Role::Guest => "Guest",
    }
}

fn invite_state(i: &Invite, now_ms: i64) -> (&'static str, bool) {
    if i.revoked_at_ms.is_some() {
        ("Revoked", false)
    } else if i.used_at_ms.is_some() {
        ("Used", false)
    } else if i.expires_at_ms <= now_ms {
        ("Expired", false)
    } else {
        ("Active", true)
    }
}

/// Members, roles and invites of one workspace (owners and admins).
#[component]
pub fn WorkspaceAdmin(account_id: Uuid, workspace_id: Uuid) -> impl IntoView {
    let c = ctx();
    let ws = c.workspace(account_id, workspace_id);
    let ws_name = ws.as_ref().map(|w| w.name.clone()).unwrap_or_default();
    let mine = ws.and_then(|w| w.role);
    let me = c.me(account_id);
    let owner = mine == Some(Role::Owner);
    let members = RwSignal::new(None::<Vec<AdminMember>>);
    let invites = RwSignal::new(None::<Vec<Invite>>);
    let error = RwSignal::new(None::<String>);
    let confirm = RwSignal::new(None::<Uuid>);
    let ids = json!({"accountId": account_id, "workspaceId": workspace_id});
    let load = {
        let ids = ids.clone();
        move || {
            let ids = ids.clone();
            leptos::task::spawn_local(async move {
                match ipc::invoke::<proto::api::AdminMemberList>("admin_members", ids.clone()).await
                {
                    Ok(l) => members.set(Some(l.members)),
                    Err(e) => error.set(Some(e)),
                }
                match ipc::invoke::<proto::api::InviteList>("list_invites", ids).await {
                    Ok(l) => invites.set(Some(l.invites)),
                    Err(e) => error.set(Some(e)),
                }
            });
        }
    };
    load();
    let act = {
        let load = load.clone();
        move |cmd: &'static str, args: serde_json::Value| {
            error.set(None);
            let load = load.clone();
            leptos::task::spawn_local(async move {
                if let Err(e) = ipc::invoke::<serde_json::Value>(cmd, args).await {
                    error.set(Some(e));
                }
                load();
            });
        }
    };
    let act2 = act.clone();
    let name_for = move |id: Uuid| {
        members.with(|m| {
            m.as_ref()
                .and_then(|m| m.iter().find(|x| x.user_id == id))
                .map(|x| x.display_name.clone())
                .unwrap_or_else(|| "someone who left".into())
        })
    };
    let ws_label = ws_name.clone();
    view! {
        <Dialog label=format!("Manage {ws_name}")>
            <div class="dbody admin">
                <div class="sect"><span>{move || format!("Members ({})", members.with(|m| m.as_ref().map(Vec::len).unwrap_or(0)))}</span></div>
                <ul class="mlist">
                    {move || members.get().unwrap_or_default().into_iter().map(|m| {
                        let act = act.clone();
                        let act_remove = act.clone();
                        let user_id = m.user_id;
                        let is_me = me == Some(user_id);
                        // Admins can't change owners; only owners grant owner.
                        let locked = m.role == Role::Owner && !owner;
                        let choices: Vec<Role> = [Role::Owner, Role::Admin, Role::Member, Role::Guest]
                            .into_iter().filter(|r| owner || *r != Role::Owner || m.role == Role::Owner).collect();
                        let select_label = format!("Role for {}", m.display_name);
                        let remove_label = format!("Remove {} from {}", m.display_name, ws_label);
                        let joined = format!("Joined {}", date(m.joined_at_ms));
                        let name = m.display_name.clone();
                        view! {
                            <li class="mrow arow">
                                {avatar(user_id, &m.display_name, "s")}
                                <span class="mname">{m.display_name.clone()}{is_me.then_some(" (you)")}
                                    <span class="pres" class:online=m.online role="img"
                                        aria-label=if m.online { "Active" } else { "Away" }></span></span>
                                <span class="muted ajoined">{joined}</span>
                                // Bound to the reloaded list, so a refused change snaps back.
                                <select class="field arole" aria-label=select_label disabled=locked
                                    prop:value=move || members.with(|ms| ms.as_ref()
                                        .and_then(|ms| ms.iter().find(|x| x.user_id == user_id))
                                        .map(|x| x.role.as_str()).unwrap_or_default())
                                    on:change=move |ev| act("set_member_role", json!({"accountId": account_id,
                                        "workspaceId": workspace_id, "userId": user_id, "role": event_target_value(&ev)}))>
                                    {choices.into_iter().map(|r| view! {
                                        <option value=r.as_str() selected=r == m.role>{role_name(r)}</option>
                                    }).collect_view()}
                                </select>
                                {(!is_me && !locked).then(|| view! {
                                    <Show when=move || confirm.get() == Some(user_id) fallback=move || {
                                        let remove_label = remove_label.clone();
                                        view! {
                                            <button class="plink danger" aria-label=remove_label
                                                on:click=move |_| confirm.set(Some(user_id))>"Remove"</button>
                                        }
                                    }>
                                        {
                                            let act_remove = act_remove.clone();
                                            let name = name.clone();
                                            view! {
                                                <span class="aconfirm" role="group" aria-label=format!("Remove {name}?")>
                                                    <button class="btn danger" on:click=move |_| {
                                                        confirm.set(None);
                                                        act_remove("remove_workspace_member", json!({"accountId": account_id,
                                                            "workspaceId": workspace_id, "userId": user_id}));
                                                    }>"Remove"</button>
                                                    <button class="btn" on:click=move |_| confirm.set(None)>"Keep"</button>
                                                </span>
                                            }
                                        }
                                    </Show>
                                })}
                            </li>
                        }
                    }).collect_view()}
                </ul>
                <p class="muted">"Removing someone ends their access to every channel here at once. Guests see only the channels they're added to."</p>
                <div class="sect"><span>"Invites"</span>
                    <button class="plink" on:click=move |_| c.overlay.set(Overlay::Invite { account_id, workspace_id })>"Invite people"</button></div>
                <ul class="mlist">
                    {move || {
                        let list = invites.get().unwrap_or_default();
                        if list.is_empty() {
                            return view! { <li class="muted">"No invites yet."</li> }.into_any();
                        }
                        let now = js_sys::Date::now() as i64;
                        list.into_iter().map(|i| {
                            let act = act2.clone();
                            let (state, live) = invite_state(&i, now);
                            let by = name_for(i.created_by);
                            let detail = match i.used_by {
                                Some(u) if i.used_at_ms.is_some() => format!("Used by {}", name_for(u)),
                                _ => format!("By {by}, until {}", date(i.expires_at_ms)),
                            };
                            let invite_id = i.id;
                            view! {
                                <li class="mrow arow">
                                    <span class="mname">{format!("{} invite", role_name(i.role))}</span>
                                    <span class="muted">{detail}</span>
                                    <span class="chip" class:on=live>{state}</span>
                                    {live.then(|| view! {
                                        <button class="plink danger" aria-label=format!("Revoke the {} invite made {}", role_name(i.role).to_lowercase(), date(i.created_at_ms))
                                            on:click=move |_| act("revoke_invite", json!({"accountId": account_id,
                                                "workspaceId": workspace_id, "inviteId": invite_id}))>"Revoke"</button>
                                    })}
                                </li>
                            }
                        }).collect_view().into_any()
                    }}
                </ul>
                {move || error.get().map(|e| view! { <p class="error" role="alert">{e}</p> })}
            </div>
            <footer class="dfoot">
                <button class="btn" on:click=move |_| c.overlay.set(Overlay::None)>"Done"</button>
            </footer>
        </Dialog>
    }
}

/// The account-settings button for the sidebar footer.
pub fn settings_button(account_id: Uuid) -> impl IntoView {
    let c = ctx();
    view! {
        <button class="iconbtn" aria-label="Account settings" title="Account settings"
            on:click=move |_| c.overlay.set(Overlay::Account(account_id))>{icon(Icon::Settings)}</button>
    }
}
