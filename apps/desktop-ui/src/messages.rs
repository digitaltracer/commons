//! The message list: a bounded window over the conversation, kept by the core.
use leptos::html::Div;
use leptos::prelude::*;
use proto::view::{MessageView, SendState};
use proto::window::{Applied, MAX_WINDOW_ROWS, WindowBatch, WindowClient, WindowRow, WindowSpec};
use serde_json::json;
use uuid::Uuid;
use wasm_bindgen::JsCast;

use crate::ipc;
use crate::markdown;
use crate::spike;

/// Rows to shift the window by when the reader reaches either end.
const STEP: u32 = 100;

fn time_label(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

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
    #[prop(optional)] on_thread: Option<Callback<Uuid>>,
    #[prop(optional)] on_edit: Option<Callback<MessageView>>,
) -> impl IntoView {
    let rows: RwSignal<Vec<WindowRow>> = RwSignal::new(vec![]);
    let total = RwSignal::new(0u32);
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

    view! {
        <div class="messages" node_ref=scroller on:scroll=on_scroll role="log" aria-live="polite"
             aria-label="Messages" tabindex="0">
            <Show when=move || rows.with(|r| r.is_empty())>
                <p class="empty">"No messages yet."</p>
            </Show>
            <For each=move || rows.get() key=|r| (r.key, r.version) let:row>
                <Message account_id=account_id conversation_id=conversation_id row=row
                         on_thread=on_thread on_edit=on_edit in_thread=thread_root_id.is_some() />
            </For>
        </div>
    }
}

#[component]
fn Message(
    account_id: Uuid,
    conversation_id: Uuid,
    row: WindowRow,
    on_thread: Option<Callback<Uuid>>,
    on_edit: Option<Callback<MessageView>>,
    in_thread: bool,
) -> impl IntoView {
    let m = row.message;
    let id = m.id;
    let class = match m.state {
        SendState::Sent => "msg",
        SendState::Sending => "msg sending",
        SendState::Failed { .. } => "msg failed",
    };
    let state = match &m.state {
        SendState::Sent => None,
        SendState::Sending => Some(view! { <span class="meta">" sending…"</span> }.into_any()),
        SendState::Failed { reason } => {
            let reason = reason.clone();
            Some(
                view! {
                    <span class="meta">" Not sent: " {reason}</span>
                    <span class="actions">
                        <button on:click=move |_| ipc::command("retry_send", json!({"accountId": account_id, "id": id}))>"Retry"</button>
                        <button on:click=move |_| ipc::command("discard_send", json!({"accountId": account_id, "id": id}))>"Discard"</button>
                    </span>
                }
                .into_any(),
            )
        }
    };
    let replies = m.reply_count;
    let thread_button = (!in_thread).then(|| {
        on_thread.map(|cb| {
            let label = if replies > 0 {
                format!("{replies} replies")
            } else {
                "Reply in thread".to_string()
            };
            view! { <button on:click=move |_| cb.run(id)>{label}</button> }
        })
    });
    let reactions = m
        .reactions
        .iter()
        .map(|r| {
            let emoji = r.emoji.clone();
            let label = format!("{} {}", r.emoji, r.user_ids.len());
            view! {
                <button aria-label=format!("{} reactions: {}", r.user_ids.len(), r.emoji)
                    on:click=move |_| ipc::command("toggle_reaction", json!({
                        "accountId": account_id, "conversationId": conversation_id,
                        "messageId": id, "emoji": emoji.clone()}))>{label}</button>
            }
        })
        .collect_view();
    let attachments = m
        .attachments
        .iter()
        .map(|a| {
            let thumb = a.thumbnail_url.clone().map(|t| view! { <img src=t alt=a.name.clone() width="160" /> });
            view! { <div class="attachment">{thumb}<a href=a.url.clone() download=a.name.clone()>{a.name.clone()}</a></div> }
        })
        .collect_view();
    let mine = m.mine && matches!(m.state, SendState::Sent);
    let edit_target = m.clone();
    let owner_actions = mine.then(|| {
        view! {
            {on_edit.map(|cb| {
                let target = edit_target.clone();
                view! { <button on:click=move |_| cb.run(target.clone())>"Edit"</button> }
            })}
            <button on:click=move |_| ipc::command("delete_message", json!({
                "accountId": account_id, "conversationId": conversation_id, "messageId": id}))>"Delete"</button>
        }
    });
    view! {
        <article class=class data-key=id.to_string()>
            <span class="who">{m.author_name.clone()}</span>
            <time class="when">{time_label(m.created_at_ms)}</time>
            {m.edited.then(|| view! { <span class="meta">" (edited)"</span> })}
            {state}
            <div class="body">{markdown::body(&m.body)}</div>
            {attachments}
            <div class="reactions">{reactions}</div>
            <div class="actions">
                {thread_button}
                <button on:click=move |_| ipc::command("toggle_reaction", json!({
                    "accountId": account_id, "conversationId": conversation_id,
                    "messageId": id, "emoji": "👍"}))>"React 👍"</button>
                {owner_actions}
            </div>
        </article>
    }
}

#[component]
pub fn Composer(
    account_id: Uuid,
    conversation_id: Uuid,
    #[prop(optional)] thread_root_id: Option<Uuid>,
    #[prop(into)] label: String,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let last_typing = StoredValue::new(0f64);
    let send = move || {
        let body = text.get_untracked();
        if body.trim().is_empty() {
            return;
        }
        text.set(String::new());
        ipc::command(
            "send_message",
            json!({"accountId": account_id, "conversationId": conversation_id,
                   "body": body, "threadRootId": thread_root_id}),
        );
    };
    let attach = move |_| {
        ipc::command(
            "attach_file",
            json!({"accountId": account_id, "conversationId": conversation_id, "threadRootId": thread_root_id}),
        );
    };
    view! {
        <form class="composer" on:submit=move |ev| { ev.prevent_default(); send(); }>
            <label class="visually-hidden" for=format!("composer-{conversation_id}")>{label.clone()}</label>
            <textarea id=format!("composer-{conversation_id}") placeholder=label rows="1"
                prop:value=move || text.get()
                on:input=move |ev| {
                    text.set(event_target_value(&ev));
                    let now = ipc::now_ms();
                    if now - last_typing.get_value() > 3000.0 {
                        last_typing.set_value(now);
                        ipc::command("typing", json!({"accountId": account_id, "conversationId": conversation_id}));
                    }
                }
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" && !ev.shift_key() {
                        ev.prevent_default();
                        send();
                    }
                }
            ></textarea>
            <button type="button" on:click=attach aria-label="Attach a file">"Attach"</button>
            <button type="submit" class="primary">"Send"</button>
        </form>
    }
}
