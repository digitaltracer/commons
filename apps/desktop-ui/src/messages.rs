//! The message list: a bounded window over the conversation, kept by the core.
use leptos::html::{Div, Textarea};
use leptos::prelude::*;
use proto::model::{Entity, Member};
use proto::view::{ConnectionState, SendState};
use proto::window::{Applied, MAX_WINDOW_ROWS, WindowBatch, WindowClient, WindowRow, WindowSpec};
use serde_json::json;
use uuid::Uuid;
use wasm_bindgen::JsCast;

use crate::emoji::EmojiPicker;
use crate::ipc;
use crate::markdown::{self, Reader, mentions_me};
use crate::spike;
use crate::state::{Aside, ctx};
use crate::ui::{self, Icon, avatar, icon, sized};

/// Rows to shift the window by when the reader reaches either end.
const STEP: u32 = 100;

thread_local! {
    static LAST_READ: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// Marks the conversation read at most once a second while the reader is at the bottom.
fn mark_read_soon(account_id: Uuid, conversation_id: Uuid) {
    let now = ipc::now_ms();
    if now - LAST_READ.with(|l| l.get()) < 1000.0 {
        return;
    }
    LAST_READ.with(|l| l.set(now));
    ipc::command(
        "mark_read",
        json!({"accountId": account_id, "conversationId": conversation_id}),
    );
}

#[component]
pub fn MessageList(
    account_id: Uuid,
    conversation_id: Uuid,
    #[prop(optional)] thread_root_id: Option<Uuid>,
    #[prop(into)] label: String,
) -> impl IntoView {
    let rows: RwSignal<Vec<WindowRow>> = RwSignal::new(vec![]);
    let total = RwSignal::new(0u32);
    let loaded = RwSignal::new(false);
    let client = StoredValue::new(WindowClient::new());
    let sub_id = StoredValue::new(None::<u64>);
    let offset = StoredValue::new(0u32);
    let pinned = StoredValue::new(true);
    let anchor = StoredValue::new(None::<(Uuid, f64)>);
    let scroller: NodeRef<Div> = NodeRef::new();

    let spec = move |off: u32| WindowSpec {
        account_id,
        conversation_id,
        thread_root_id,
        offset_from_end: off,
        limit: MAX_WINDOW_ROWS,
    };

    let after_render = move || {
        ipc::next_frame(move || {
            let Some(el) = scroller.get_untracked() else {
                return;
            };
            if let Some((key, top)) = anchor.get_value() {
                anchor.set_value(None);
                if let Some(row) = el
                    .query_selector(&format!("[data-key='{key}']"))
                    .ok()
                    .flatten()
                    .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
                {
                    el.set_scroll_top(row.offset_top() - top as i32);
                    return;
                }
            }
            if pinned.get_value() {
                el.set_scroll_top(el.scroll_height());
            }
        });
    };

    let on_batch = move |batch: WindowBatch| {
        let t0 = ipc::now_ms();
        let is_init = matches!(batch, WindowBatch::Init { .. });
        let sid = batch.sub_id();
        let result = client.try_update_value(|c| {
            let r = c.apply(batch);
            (r, c.generation, c.rows.clone(), c.total)
        });
        let Some((applied, generation, new_rows, new_total)) = result else {
            return;
        };
        match applied {
            Applied::Ack(revision) => {
                if pinned.get_value() && thread_root_id.is_none() {
                    mark_read_soon(account_id, conversation_id);
                }
                rows.set(new_rows);
                total.set(new_total);
                loaded.set(true);
                after_render();
                ipc::command(
                    "ack_window",
                    json!({"accountId": account_id, "subId": sid, "generation": generation, "revision": revision}),
                );
                ipc::next_frame(move || spike::record_batch(ipc::now_ms() - t0, is_init));
            }
            Applied::NeedsReset => ipc::command(
                "reset_window",
                json!({"accountId": account_id, "subId": sid}),
            ),
            Applied::Stale => {}
        }
    };

    leptos::task::spawn_local(async move {
        let r: Result<u64, String> = ipc::invoke_with_channel(
            "subscribe_window",
            json!({"spec": spec(0)}),
            "onBatch",
            on_batch,
        )
        .await;
        match r {
            Ok(id) => sub_id.set_value(Some(id)),
            Err(e) => ipc::warn(e),
        }
    });
    on_cleanup(move || {
        if let Some(id) = sub_id.get_value() {
            ipc::command(
                "unsubscribe_window",
                json!({"accountId": account_id, "subId": id}),
            );
        }
    });

    let move_window = move |new_offset: u32| {
        let Some(id) = sub_id.get_value() else { return };
        // Keep the first visible row where it is while the window shifts under it.
        if let Some(el) = scroller.get_untracked() {
            let top = el.scroll_top() as f64;
            let first = rows.with_untracked(|rs| {
                rs.iter().find_map(|r| {
                    el.query_selector(&format!("[data-key='{}']", r.key))
                        .ok()
                        .flatten()
                        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
                        .filter(|e| e.offset_top() as f64 + e.offset_height() as f64 > top)
                        .map(|e| (r.key, e.offset_top() as f64 - top))
                })
            });
            anchor.set_value(first);
        }
        offset.set_value(new_offset);
        ipc::command(
            "move_window",
            json!({"accountId": account_id, "subId": id, "spec": spec(new_offset)}),
        );
    };

    let on_scroll = move |_| {
        let Some(el) = scroller.get_untracked() else {
            return;
        };
        let top = el.scroll_top();
        let bottom_gap = el.scroll_height() - top - el.client_height();
        pinned.set_value(bottom_gap < 40 && offset.get_value() == 0);
        let loaded = rows.with_untracked(|r| r.len() as u32);
        let off = offset.get_value();
        if top < 200 && off + loaded < total.get_untracked() && anchor.get_value().is_none() {
            move_window(off + STEP);
        } else if bottom_gap < 200 && off > 0 && anchor.get_value().is_none() {
            move_window(off.saturating_sub(STEP));
        }
    };
    spike::register_scroller(scroller);

    // Day dividers sit above the first row of each day; a thread's root gets a reply count.
    let shown = move || {
        let rs = rows.get();
        let mut prev: Option<i64> = None;
        let mut out = Vec::with_capacity(rs.len());
        for (i, r) in rs.into_iter().enumerate() {
            let at = r.message.created_at_ms;
            let day = match prev {
                Some(p) if ui::same_day(p, at) => None,
                _ => Some(ui::day_label(at)),
            };
            prev = Some(at);
            let divider = match (thread_root_id, i) {
                (Some(_), 0) => None,
                (Some(_), 1) => Some(String::new()),
                (None, _) => day,
                _ => None,
            };
            out.push((r, divider));
        }
        out
    };
    let replies = move || rows.with(|r| r.len().saturating_sub(1));

    view! {
        <div class="list" node_ref=scroller on:scroll=on_scroll role="log" aria-live="polite"
             aria-label=label tabindex="0">
            <div class="spacer"></div>
            {move || (loaded.get() && rows.with(|r| r.is_empty())).then(|| empty(account_id, conversation_id, thread_root_id))}
            <For each=shown key=|(r, d)| (r.key, r.version, d.clone()) let:item>
                {item.1.map(|d| if d.is_empty() {
                    view! { <div class="tsep">{move || format!("{} {}", replies(), if replies() == 1 { "reply" } else { "replies" })}</div> }.into_any()
                } else {
                    view! { <div class="day" role="separator">{d}</div> }.into_any()
                })}
                <Message account_id=account_id conversation_id=conversation_id row=item.0
                         in_thread=thread_root_id.is_some() />
            </For>
        </div>
    }
}

/// An empty list. Away from the server, it says nothing is saved rather than that nothing exists.
fn empty(account_id: Uuid, conversation_id: Uuid, thread_root_id: Option<Uuid>) -> AnyView {
    let c = ctx();
    let connection = c.connection(account_id);
    if thread_root_id.is_some() || connection == ConnectionState::Online {
        return view! { <p class="empty">"No messages yet."</p> }.into_any();
    }
    let title = c
        .conversation(crate::state::Selection {
            account_id,
            conversation_id,
        })
        .map(|(_, cv)| cv.title)
        .filter(|t| t.starts_with('#'))
        .unwrap_or_else(|| "this conversation".into());
    let then = if connection == ConnectionState::SignedOut {
        "Its messages load after you sign in."
    } else {
        "Its messages load when Commons reconnects."
    };
    view! {
        <div class="emptyst">
            <span class="emptyico">{sized(Icon::Compose, 22)}</span>
            <h2>{format!("Nothing from {title} is saved on this Mac")}</h2>
            <p>{then}</p>
        </div>
    }
    .into_any()
}

#[component]
fn Message(
    account_id: Uuid,
    conversation_id: Uuid,
    row: WindowRow,
    in_thread: bool,
) -> impl IntoView {
    let c = ctx();
    let m = row.message;
    let id = m.id;
    let me = c.me(account_id).unwrap_or_default();
    let reader = Reader { account_id, me };
    let picking = RwSignal::new(None::<(f64, f64)>);
    let offline = move || c.connection(account_id) != ConnectionState::Online;

    let pending = matches!(m.state, SendState::Sending);
    let failed = matches!(m.state, SendState::Failed { .. });
    let state = match &m.state {
        SendState::Sent => None,
        SendState::Sending => {
            let body = m.body.clone();
            Some(
                view! {
                    <div class="pstate">
                        {sized(Icon::Clock, 14)}
                        {move || if offline() {
                            view! {
                                <span>{move || if c.connection(account_id) == ConnectionState::SignedOut {
                                    "Waiting to send. It goes out after you sign in."
                                } else { "Waiting to send. It goes out once when you reconnect." }}</span>
                                <button class="plink" on:click={
                                    let body = body.clone();
                                    move |_| {
                                        ipc::command("discard_send", json!({"accountId": account_id, "id": id}));
                                        c.refill.set(Some((conversation_id, body.clone())));
                                    }
                                }>"Edit"</button>
                                <button class="plink" on:click=move |_| ipc::command("discard_send", json!({"accountId": account_id, "id": id}))>"Delete"</button>
                            }.into_any()
                        } else {
                            view! { <span>"Sending…"</span> }.into_any()
                        }}
                    </div>
                }
                .into_any(),
            )
        }
        SendState::Failed { reason } => {
            let reason = reason.clone();
            Some(
                view! {
                    <div class="pstate failed" role="alert">
                        {sized(Icon::Alert, 14)}
                        <span>"Not sent: " {reason}</span>
                        <button class="plink" on:click=move |_| ipc::command("retry_send", json!({"accountId": account_id, "id": id}))>"Retry"</button>
                        <button class="plink" on:click=move |_| ipc::command("discard_send", json!({"accountId": account_id, "id": id}))>"Discard"</button>
                    </div>
                }
                .into_any(),
            )
        }
    };

    let react = move |emoji: String| {
        picking.set(None);
        ipc::command(
            "toggle_reaction",
            json!({"accountId": account_id, "conversationId": conversation_id, "messageId": id, "emoji": emoji}),
        );
    };
    let sent = matches!(m.state, SendState::Sent) && !m.deleted;
    let reactions = m
        .reactions
        .iter()
        .map(|r| {
            let emoji = r.emoji.clone();
            let mine = r.user_ids.contains(&me);
            let n = r.user_ids.len();
            let label = format!(
                "{} {n}{}",
                r.emoji,
                if mine { ", including you" } else { "" }
            );
            view! {
                <button class="re" class:mine=mine aria-label=label aria-pressed=mine.to_string()
                    on:click=move |_| react(emoji.clone())>{format!("{} {n}", r.emoji)}</button>
            }
        })
        .collect_view();
    let has_reactions = !m.reactions.is_empty();

    let attachments = m
        .attachments
        .iter()
        .map(|a| {
            let file_id = a.file_id;
            let name = a.name.clone();
            let save = move |_| {
                let name = name.clone();
                leptos::task::spawn_local(async move {
                    let args = json!({"accountId": account_id, "fileId": file_id, "name": name});
                    if let Err(e) = ipc::invoke::<bool>("save_file", args).await {
                        ipc::warn(format!("download: {e}"));
                    }
                });
            };
            let sub = format!("{} · {}", ui::kind_label(&a.mime, &a.name), ui::size(a.size));
            let dl_label = format!("Download {}", a.name);
            match a.thumbnail_url.clone() {
                Some(t) => view! {
                    <div class="card thumbcard">
                        <div class="thumb">{
                            // Offline, or no longer readable: show the file icon, not a broken image.
                            let broken = RwSignal::new(false);
                            let alt = format!("Thumbnail of {}", a.name);
                            move || if broken.get() {
                                view! { <span class="ficon">{sized(Icon::File, 18)}</span> }.into_any()
                            } else {
                                view! { <img src=t.clone() alt=alt.clone() on:error=move |_| broken.set(true) /> }.into_any()
                            }
                        }</div>
                        <div class="cardrow">
                            <div class="fmeta"><div class="fname">{a.name.clone()}</div><div class="fsub">{sub}</div></div>
                            <button class="iconbtn" aria-label=dl_label on:click=save>{icon(Icon::Download)}</button>
                        </div>
                    </div>
                }.into_any(),
                None => view! {
                    <div class="card fcard">
                        <span class="ficon">{sized(Icon::File, 18)}</span>
                        <div class="fmeta"><div class="fname">{a.name.clone()}</div><div class="fsub">{sub}</div></div>
                        <button class="iconbtn" aria-label=dl_label on:click=save>{icon(Icon::Download)}</button>
                    </div>
                }.into_any(),
            }
        })
        .collect_view();
    let has_files = !m.attachments.is_empty();

    let replies = m.reply_count;
    let thread_summary = (!in_thread && replies > 0).then(|| {
        view! {
            <button class="thr" on:click=move |_| c.aside.set(Aside::Thread(id))>
                <b>{format!("{replies} {}", if replies == 1 { "reply" } else { "replies" })}</b>
                <span>"View thread"</span>
            </button>
        }
    });

    let mine = m.mine && sent;
    let edit_target = m.clone();
    let class = if m.deleted {
        "msg deleted"
    } else if mentions_me(&m.entities, me) && !m.mine {
        "msg mention"
    } else {
        "msg"
    };
    let author = m.author_name.clone();
    let body = (!m.deleted).then(|| {
        view! {
            <div class="body">{markdown::body(&m.body, &m.entities, reader, m.edited)}</div>
        }
    });
    let aria = if pending {
        "Your message, waiting to send".to_string()
    } else {
        format!("{} at {}", m.author_name, ui::clock(m.created_at_ms))
    };

    view! {
        <div class=class class:pending=pending class:failed=failed data-key=id.to_string() aria-label=aria>
            {avatar(m.author_id, &m.author_name, "")}
            <div class="mc">
                <div class="mh">
                    <span class="name" class:dim=m.deleted on:click=move |_| c.overlay.set(crate::state::Overlay::Person { account_id, user_id: m.author_id })>{author}</span>
                    <span class="time">{ui::clock(m.created_at_ms)}</span>
                </div>
                {body}
                {m.deleted.then(|| view! { <div class="tomb">{sized(Icon::Trash, 14)}"This message was deleted."</div> })}
                {has_files.then(|| view! { <div class="att">{attachments}</div> })}
                {has_reactions.then(|| view! {
                    <div class="reacts">
                        {reactions}
                        <button class="re add" aria-label="Add reaction" on:click=move |ev| picking.set(Some(anchor_of(&ev)))>{sized(Icon::Smile, 15)}</button>
                    </div>
                })}
                {thread_summary}
                {state}
            </div>
            {sent.then(|| view! {
                <div class="actions" role="toolbar" aria-label="Message actions">
                    <button class="iconbtn" aria-label="Add reaction" on:click=move |ev| picking.set(Some(anchor_of(&ev)))>{icon(Icon::Smile)}</button>
                    {(!in_thread).then(|| view! {
                        <button class="iconbtn" aria-label="Reply in thread" on:click=move |_| c.aside.set(Aside::Thread(id))>{icon(Icon::Reply)}</button>
                    })}
                    {mine.then(|| {
                        let target = edit_target.clone();
                        view! {
                            <button class="iconbtn" aria-label="Edit message" on:click=move |_| c.editing.set(Some(target.clone()))>{icon(Icon::Edit)}</button>
                            <button class="iconbtn" aria-label="Delete message" on:click=move |_| ipc::command("delete_message", json!({
                                "accountId": account_id, "conversationId": conversation_id, "messageId": id}))>{icon(Icon::Trash)}</button>
                        }
                    })}
                </div>
            })}
            {move || picking.get().map(|at| view! {
                <EmojiPicker account_id=account_id at=at on_pick=Callback::new(react)
                    on_close=Callback::new(move |_| picking.set(None)) />
            })}
        </div>
    }
}

/// The bottom-right corner of the clicked button, where a floating picker attaches.
fn anchor_of(ev: &web_sys::MouseEvent) -> (f64, f64) {
    ev.current_target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .map(|e| {
            let r = e.get_bounding_client_rect();
            (r.right(), r.bottom())
        })
        .unwrap_or((0.0, 0.0))
}

/// An `@` name being typed: where the `@` is (byte offset) and the text after it.
fn mention_query(text: &str, caret: usize) -> Option<(usize, String)> {
    let before = &text[..caret];
    let at = before.rfind('@')?;
    if at > 0 && !before[..at].ends_with(|c: char| c.is_whitespace() || "([{".contains(c)) {
        return None;
    }
    let q = &before[at + 1..];
    if q.len() > 32 || q.contains('\n') || q.starts_with(' ') {
        return None;
    }
    Some((at, q.to_string()))
}

fn matches_query(name: &str, q: &str) -> bool {
    let q = q.to_lowercase();
    let n = name.to_lowercase();
    n.starts_with(&q) || n.split_whitespace().any(|w| w.starts_with(&q))
}

/// Mention entities for the names picked from autocomplete that are still in the text.
fn picked_entities(body: &str, picked: &[(Uuid, String)]) -> Vec<Entity> {
    let mut taken: Vec<(usize, usize)> = vec![];
    let mut out = vec![];
    for (user_id, name) in picked {
        let needle = format!("@{name}");
        let hit = body
            .match_indices(&needle)
            .map(|(s, _)| (s, s + needle.len()))
            .find(|(s, e)| {
                body[*e..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric())
                    && taken.iter().all(|(ts, te)| e <= ts || s >= te)
            });
        if let Some((s, e)) = hit {
            taken.push((s, e));
            out.push(Entity::Mention {
                user_id: *user_id,
                start: s as u32,
                end: e as u32,
            });
        }
    }
    out
}

fn textarea(r: NodeRef<Textarea>) -> Option<web_sys::HtmlTextAreaElement> {
    r.get_untracked()
}

/// Replaces bytes `from..to` of the composer text and puts the caret after `insert`.
fn splice(
    text: RwSignal<String>,
    r: NodeRef<Textarea>,
    from: usize,
    to: usize,
    insert: &str,
    caret_back: usize,
) {
    let mut t = text.get_untracked();
    let to = to.min(t.len());
    let from = from.min(to);
    t.replace_range(from..to, insert);
    let caret = from + insert.len() - caret_back;
    let pos = ui::byte_to_utf16(&t, caret);
    text.set(t);
    if let Some(el) = textarea(r) {
        ipc::next_frame(move || {
            let _ = el.focus();
            let _ = el.set_selection_range(pos, pos);
        });
    }
}

fn selection(text: &str, r: NodeRef<Textarea>) -> (usize, usize) {
    textarea(r)
        .map(|el| {
            let s = el.selection_start().ok().flatten().unwrap_or(0);
            let e = el.selection_end().ok().flatten().unwrap_or(s);
            (ui::utf16_to_byte(text, s), ui::utf16_to_byte(text, e))
        })
        .unwrap_or((text.len(), text.len()))
}

#[component]
pub fn Composer(
    account_id: Uuid,
    conversation_id: Uuid,
    #[prop(optional)] thread_root_id: Option<Uuid>,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
) -> impl IntoView {
    let c = ctx();
    let text = RwSignal::new(String::new());
    let picked = StoredValue::new(Vec::<(Uuid, String)>::new());
    let area: NodeRef<Textarea> = NodeRef::new();
    let last_typing = StoredValue::new(0f64);
    let members = RwSignal::new(Vec::<Member>::new());
    let ac = RwSignal::new(None::<(usize, String)>);
    let ac_index = RwSignal::new(0usize);
    let emoji = RwSignal::new(false);
    let me = c.me(account_id).unwrap_or_default();
    let offline = move || c.connection(account_id) != ConnectionState::Online;

    // Autocomplete offers the conversation's members; the workspace list stands in offline.
    leptos::task::spawn_local(async move {
        let args = json!({"accountId": account_id, "conversationId": conversation_id});
        match ipc::invoke::<Vec<Member>>("conversation_members", args).await {
            Ok(m) => members.set(m),
            Err(_) => {
                let ws = c
                    .conversation(crate::state::Selection {
                        account_id,
                        conversation_id,
                    })
                    .map(|(w, _)| w.id);
                if let Some(ws) = ws
                    && let Ok(m) = ipc::invoke::<Vec<Member>>(
                        "workspace_members",
                        json!({"accountId": account_id, "workspaceId": ws}),
                    )
                    .await
                {
                    members.set(m);
                }
            }
        }
    });

    Effect::new(move |_| {
        if let Some((conv, body)) = c.refill.get()
            && conv == conversation_id
            && thread_root_id.is_none()
        {
            c.refill.set(None);
            text.set(body);
        }
    });

    let suggestions = move || {
        let Some((_, q)) = ac.get() else {
            return vec![];
        };
        members.with(|ms| {
            ms.iter()
                .filter(|m| m.user_id != me && matches_query(&m.display_name, &q))
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
        })
    };

    let refresh_ac = move || {
        let t = text.get_untracked();
        let (caret, end) = selection(&t, area);
        let q = (caret == end).then(|| mention_query(&t, caret)).flatten();
        if q.is_none() || q.as_ref().map(|x| &x.1) != ac.get_untracked().as_ref().map(|x| &x.1) {
            ac_index.set(0);
        }
        ac.set(q);
    };

    let pick = move |m: Member| {
        let Some((at, _)) = ac.get_untracked() else {
            return;
        };
        let t = text.get_untracked();
        let (caret, _) = selection(&t, area);
        picked.update_value(|p| p.push((m.user_id, m.display_name.clone())));
        ac.set(None);
        splice(text, area, at, caret, &format!("@{} ", m.display_name), 0);
    };

    let send = move || {
        let body = text.get_untracked();
        if body.trim().is_empty() {
            return;
        }
        let mentions = picked.with_value(|p| picked_entities(&body, p));
        text.set(String::new());
        picked.set_value(vec![]);
        ac.set(None);
        ipc::command(
            "send_message",
            json!({"accountId": account_id, "conversationId": conversation_id,
                   "body": body, "threadRootId": thread_root_id, "mentions": mentions}),
        );
    };

    let wrap = move |before: &'static str, after: &'static str| {
        let t = text.get_untracked();
        let (s, e) = selection(&t, area);
        let inner = t[s..e].to_string();
        splice(
            text,
            area,
            s,
            e,
            &format!("{before}{inner}{after}"),
            after.len(),
        );
    };
    let list = move || {
        let t = text.get_untracked();
        let (s, _) = selection(&t, area);
        let line = t[..s].rfind('\n').map_or(0, |i| i + 1);
        splice(text, area, line, line, "- ", 0);
        let _ = s;
    };

    let attach_error = RwSignal::new(None::<String>);
    let attaching = RwSignal::new(false);
    let attach = move |_| {
        attach_error.set(None);
        attaching.set(true);
        leptos::task::spawn_local(async move {
            let r: Result<Option<Uuid>, String> = ipc::invoke(
                "attach_file",
                json!({"accountId": account_id, "conversationId": conversation_id, "threadRootId": thread_root_id}),
            )
            .await;
            attaching.set(false);
            if let Err(e) = r {
                attach_error.set(Some(format!("Could not attach: {e}")));
            }
        });
    };
    let rows =
        move || text.with(|t| t.lines().count().clamp(1, 8) + usize::from(t.ends_with('\n')));
    let id = format!("composer-{conversation_id}-{}", thread_root_id.is_some());
    let placeholder2 = placeholder.clone();
    let full = thread_root_id.is_none();

    view! {
        <div class="comp">
            {full.then(|| view! {
                <div class="ctool" role="toolbar" aria-label="Formatting">
                    <button type="button" class="iconbtn" aria-label="Bold" on:click=move |_| wrap("**", "**")>{icon(Icon::Bold)}</button>
                    <button type="button" class="iconbtn" aria-label="Italic" on:click=move |_| wrap("_", "_")>{icon(Icon::Italic)}</button>
                    <button type="button" class="iconbtn" aria-label="Code" on:click=move |_| wrap("`", "`")>{icon(Icon::Code)}</button>
                    <button type="button" class="iconbtn" aria-label="Link" on:click=move |_| wrap("[", "](https://)")>{icon(Icon::Link)}</button>
                    <button type="button" class="iconbtn" aria-label="Bulleted list" on:click=move |_| list()>{icon(Icon::List)}</button>
                </div>
            })}
            <div class="cwrap">
                <label class="visually-hidden" for=id.clone()>{label.clone()}</label>
                <textarea id=id class="ctext" node_ref=area rows=rows
                    placeholder=move || match c.connection(account_id) {
                        ConnectionState::Online => placeholder2.clone(),
                        ConnectionState::SignedOut => format!("{placeholder2}. Sends after you sign in"),
                        _ => format!("{placeholder2}. Sends when you're back online"),
                    }
                    aria-autocomplete="list" aria-expanded=move || (!suggestions().is_empty()).to_string()
                    prop:value=move || text.get()
                    on:input=move |ev| {
                        text.set(event_target_value(&ev));
                        refresh_ac();
                        let now = ipc::now_ms();
                        if now - last_typing.get_value() > 3000.0 {
                            last_typing.set_value(now);
                            ipc::command("typing", json!({"accountId": account_id, "conversationId": conversation_id}));
                        }
                    }
                    on:click=move |_| refresh_ac()
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        let s = suggestions();
                        if !s.is_empty() {
                            match ev.key().as_str() {
                                "ArrowDown" => { ev.prevent_default(); ac_index.update(|i| *i = (*i + 1) % s.len()); return; }
                                "ArrowUp" => { ev.prevent_default(); ac_index.update(|i| *i = (*i + s.len() - 1) % s.len()); return; }
                                "Enter" | "Tab" => { ev.prevent_default(); pick(s[ac_index.get_untracked().min(s.len() - 1)].clone()); return; }
                                "Escape" => { ev.prevent_default(); ac.set(None); return; }
                                _ => {}
                            }
                        }
                        if ev.key() == "Enter" && !ev.shift_key() {
                            ev.prevent_default();
                            send();
                        }
                    }
                ></textarea>
                <Show when=move || !suggestions().is_empty()>
                    <ul class="ac" role="listbox" aria-label="People to mention">
                        {move || suggestions().into_iter().enumerate().map(|(i, m)| {
                            let online = c.online(account_id, m.user_id);
                            let target = m.clone();
                            view! {
                                <li role="option" aria-selected=move || (ac_index.get() == i).to_string()
                                    class:on=move || ac_index.get() == i
                                    on:mousedown=move |ev| { ev.prevent_default(); pick(target.clone()); }>
                                    {avatar(m.user_id, &m.display_name, "s")}
                                    <span class="acname">{m.display_name.clone()}</span>
                                    <span class="pres" class:online=online aria-label=if online { "online" } else { "offline" }></span>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </Show>
            </div>
            <div class="cfoot">
                <button type="button" class="iconbtn" on:click=attach aria-label="Attach a file"
                    disabled=move || attaching.get() || offline()>{icon(Icon::Attach)}</button>
                {full.then(|| view! {
                    <button type="button" class="iconbtn" aria-label="Mention someone" on:click=move |_| {
                        let t = text.get_untracked();
                        let (s, e) = selection(&t, area);
                        let lead = if s == 0 || t[..s].ends_with(char::is_whitespace) { "@" } else { " @" };
                        splice(text, area, s, e, lead, 0);
                        ipc::next_frame(refresh_ac);
                    }>{icon(Icon::At)}</button>
                })}
                <span class="emowrap">
                    <button type="button" class="iconbtn" aria-label="Emoji" on:click=move |_| emoji.set(true)>{icon(Icon::Smile)}</button>
                    <Show when=move || emoji.get()>
                        <EmojiPicker account_id=account_id placement="up"
                            on_pick=Callback::new(move |e: String| {
                                emoji.set(false);
                                let t = text.get_untracked();
                                let (s, en) = selection(&t, area);
                                splice(text, area, s, en, &e, 0);
                            })
                            on_close=Callback::new(move |_| emoji.set(false)) />
                    </Show>
                </span>
                {move || attaching.get().then(|| view! { <span class="hint">"Uploading…"</span> })}
                <span class="hint">{move || if c.connection(account_id) == ConnectionState::SignedOut { "Signed out · messages wait in your outbox" }
                    else if offline() { "Offline · messages wait in your outbox" }
                    else if full { "Markdown · Return to send · Shift-Return for a new line" } else { "" }}</span>
                <button type="button" class="sendbtn" aria-label=move || if offline() { "Queue message" } else { "Send" }
                    on:click=move |_| send()>{icon(Icon::Send)}</button>
            </div>
        </div>
        {move || attach_error.get().map(|e| view! { <p class="error attach-error" role="alert">{e}</p> })}
    }
}

#[component]
pub fn EditBar(account_id: Uuid, conversation_id: Uuid) -> impl IntoView {
    let c = ctx();
    let text = RwSignal::new(String::new());
    Effect::new(move |_| {
        if let Some(m) = c.editing.get() {
            text.set(m.body.clone());
        }
    });
    let save = move || {
        if let Some(m) = c.editing.get_untracked() {
            ipc::command(
                "edit_message",
                json!({"accountId": account_id, "conversationId": conversation_id,
                       "messageId": m.id, "baseRevision": m.revision, "body": text.get_untracked()}),
            );
        }
        c.editing.set(None);
    };
    view! {
        {move || c.editing.get().map(|_| view! {
            <form class="comp editbar" on:submit=move |ev| { ev.prevent_default(); save(); }>
                <div class="ebanner">{sized(Icon::Edit, 14)}"Editing your message"</div>
                <label class="visually-hidden" for="edit-box">"Edit message"</label>
                <textarea id="edit-box" class="ctext" rows="2" prop:value=move || text.get()
                    on:input=move |ev| text.set(event_target_value(&ev))
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" && !ev.shift_key() { ev.prevent_default(); save(); }
                        if ev.key() == "Escape" { c.editing.set(None); }
                    }></textarea>
                <div class="cfoot">
                    <span class="hint">"Return to save · Escape to cancel"</span>
                    <button type="button" class="btn" on:click=move |_| c.editing.set(None)>"Cancel"</button>
                    <button type="submit" class="btn primary">"Save"</button>
                </div>
            </form>
        })}
    }
}
