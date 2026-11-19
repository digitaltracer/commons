//! The message list: a bounded window over the conversation, kept by the core.
use leptos::html::{Div, Textarea};
use leptos::prelude::*;
use proto::model::{ConversationKind, Entity, Member};
use proto::view::{ConnectionState, SendState, StagedFile, StagedState};
use proto::window::{Applied, MAX_WINDOW_ROWS, WindowBatch, WindowClient, WindowRow, WindowSpec};
use serde_json::json;
use uuid::Uuid;
use wasm_bindgen::JsCast;

use crate::compose;
use crate::emoji::{self, EmojiPicker};
use crate::ipc;
use crate::layer;
use crate::markdown::{self, Reader, mentions_me};
use crate::spike;
use crate::state::{Aside, Refill, ctx};
use crate::ui::{self, Icon, avatar, icon, sized};

/// Rows to shift the window by when the reader reaches either end.
const STEP: u32 = 100;

thread_local! {
    static LAST_READ: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    static READ_LATER: std::cell::Cell<Option<(Uuid, Uuid)>> = const { std::cell::Cell::new(None) };
}

/// Marks the conversation read at most once a second, with a trailing mark so the last
/// message of a burst isn't left unread. The core ignores it while the window is inactive.
fn mark_read_soon(account_id: Uuid, conversation_id: Uuid) {
    let now = ipc::now_ms();
    let wait = 1000.0 - (now - LAST_READ.with(|l| l.get()));
    if wait > 0.0 {
        if READ_LATER
            .with(|p| p.replace(Some((account_id, conversation_id))))
            .is_none()
        {
            set_timeout(
                || {
                    if let Some((a, c)) = READ_LATER.with(|p| p.take()) {
                        mark_read_soon(a, c);
                    }
                },
                std::time::Duration::from_millis(wait as u64 + 1),
            );
        }
        return;
    }
    LAST_READ.with(|l| l.set(now));
    ipc::command(
        "mark_read",
        json!({"accountId": account_id, "conversationId": conversation_id}),
    );
}

/// The row the New line sits above: the `unread`-th message from someone else, counting back.
fn first_unread(rows: &[WindowRow], unread: u32) -> Option<Uuid> {
    let mut n = 0;
    for r in rows.iter().rev() {
        if !r.message.mine {
            n += 1;
            if n == unread {
                return Some(r.key);
            }
        }
    }
    None
}

/// Messages from others appended after the row that was last before this batch.
fn arrivals(old: &[WindowRow], new: &[WindowRow]) -> u32 {
    let Some(last) = old.last() else { return 0 };
    match new.iter().position(|r| r.key == last.key) {
        Some(i) => new[i + 1..].iter().filter(|r| !r.message.mine).count() as u32,
        None => 0,
    }
}

/// Rows from the same author this close together share one header.
const GROUP_MS: i64 = 5 * 60 * 1000;

/// "Priya Nair: the text" for the newest message from someone else in this batch.
fn arrival_text(old: &[WindowRow], new: &[WindowRow]) -> Option<String> {
    let last = old.last()?;
    let i = new.iter().position(|r| r.key == last.key)?;
    let m = &new[i + 1..].iter().rev().find(|r| !r.message.mine)?.message;
    let text: String = proto::markdown::plain_text(&m.body)
        .chars()
        .take(140)
        .collect();
    Some(format!("{}: {text}", m.author_name))
}

/// The top of a conversation: what it is, and for channels a way to add people.
fn intro(account_id: Uuid, conversation_id: Uuid) -> AnyView {
    let c = ctx();
    let Some((w, cv)) = c.conversation(crate::state::Selection {
        account_id,
        conversation_id,
    }) else {
        return ().into_any();
    };
    let (icon_view, title, about, add) = match cv.kind {
        proto::model::ConversationKind::Public => (
            sized(Icon::Hash, 22).into_any(),
            format!("This is the start of {}", cv.title),
            format!("Anyone in {} can find and join this channel.", w.name),
            true,
        ),
        proto::model::ConversationKind::Private => (
            sized(Icon::Lock, 22).into_any(),
            format!("This is the start of {}", cv.title),
            "A private channel: only people who are added can read it.".to_string(),
            true,
        ),
        proto::model::ConversationKind::Dm => (
            avatar(
                cv.peer_ids.first().copied().unwrap_or(cv.id),
                &cv.title,
                "l",
            )
            .into_any(),
            format!("This is the start of your conversation with {}", cv.title),
            "Only the two of you can read these messages.".to_string(),
            false,
        ),
        proto::model::ConversationKind::GroupDm => (
            sized(Icon::Members, 22).into_any(),
            format!("This is the start of your conversation with {}", cv.title),
            "Only the people in this group can read these messages.".to_string(),
            true,
        ),
    };
    view! {
        <div class="intro">
            <span class="introico">{icon_view}</span>
            <h2>{title}</h2>
            <p>{about}</p>
            {add.then(|| view! {
                <button class="btn ghost" on:click=move |_| c.aside.set(Aside::Members)>
                    {sized(Icon::Members, 15)}"Add people"</button>
            })}
        </div>
    }
    .into_any()
}

fn row_el(list: &web_sys::HtmlElement, key: Uuid) -> Option<web_sys::HtmlElement> {
    list.query_selector(&format!("[data-key='{key}']"))
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
}

/// Still placeholder rows until the first batch arrives.
fn skeleton() -> impl IntoView {
    let widths = [(140, 82), (110, 64), (160, 74), (96, 48)];
    view! {
        <div class="skel" aria-hidden="true">
            {widths.into_iter().map(|(name, line)| view! {
                <div class="skrow">
                    <span class="sk skav"></span>
                    <div class="skl">
                        <span class="sk" style=format!("width: {name}px")></span>
                        <span class="sk" style=format!("width: {line}%")></span>
                    </div>
                </div>
            }).collect_view()}
        </div>
    }
}

#[component]
pub fn MessageList(
    account_id: Uuid,
    conversation_id: Uuid,
    #[prop(optional)] thread_root_id: Option<Uuid>,
    #[prop(into)] label: String,
) -> impl IntoView {
    let c = ctx();
    let main = thread_root_id.is_none();
    let list_key = (conversation_id, thread_root_id);
    // Coming back to a list opens it where it was left, unless that was the end.
    let saved = c.anchors.with_value(|a| a.get(&list_key).copied());
    let rows: RwSignal<Vec<WindowRow>> = RwSignal::new(vec![]);
    let total = RwSignal::new(0u32);
    let loaded = RwSignal::new(false);
    let client = StoredValue::new(WindowClient::new());
    let sub_id = StoredValue::new(None::<u64>);
    let offset = StoredValue::new(saved.map_or(0, |a| a.offset));
    let pinned = StoredValue::new(saved.is_none());
    let last_top = StoredValue::new(0i32);
    let anchor = StoredValue::new(saved.map(|a| (a.key, a.top)));
    let scroller: NodeRef<Div> = NodeRef::new();
    // The unread count when the conversation opened fixes where the New line goes.
    let unread_at_open = if main {
        untrack(|| {
            c.conversation(crate::state::Selection {
                account_id,
                conversation_id,
            })
            .map_or(0, |(_, cv)| cv.unread)
        })
    } else {
        0
    };
    let new_from = RwSignal::new(None::<Uuid>);
    let first_batch = StoredValue::new(true);
    let open_at_new = StoredValue::new(false);
    let away = RwSignal::new(saved.is_some());
    let arrived = RwSignal::new(0u32);
    let roving = RwSignal::new(None::<Uuid>);
    let last_key = Memo::new(move |_| rows.with(|r| r.last().map(|r| r.key)));
    let announce = RwSignal::new(String::new());
    let offset_sig = RwSignal::new(offset.get_value());

    let spec = move |off: u32| WindowSpec {
        account_id,
        conversation_id,
        thread_root_id,
        offset_from_end: off,
        limit: MAX_WINDOW_ROWS,
    };

    // Reading: the main list at its newest row, in an active window, after it has opened.
    let seen = move || {
        if main
            && pinned.get_value()
            && c.active.get_untracked()
            && !first_batch.get_value()
            && !open_at_new.get_value()
        {
            mark_read_soon(account_id, conversation_id);
        }
    };

    let after_render = move || {
        ipc::next_frame(move || {
            let Some(el) = scroller.get_untracked() else {
                return;
            };
            if let Some((key, top)) = anchor.get_value() {
                anchor.set_value(None);
                if let Some(row) = row_el(&el, key) {
                    el.set_scroll_top(row.offset_top() - top as i32);
                    return;
                }
            }
            if open_at_new.get_value() {
                open_at_new.set_value(false);
                // More unread than fits on a screen: open at the New line, not the end.
                if let Some(row) = new_from.get_untracked().and_then(|k| row_el(&el, k))
                    && row.offset_top() < el.scroll_height() - el.client_height()
                {
                    pinned.set_value(false);
                    away.set(true);
                    el.set_scroll_top(row.offset_top() - 36);
                    return;
                }
            }
            if pinned.get_value() {
                el.set_scroll_top(el.scroll_height());
                seen();
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
                if first_batch.get_value() {
                    first_batch.set_value(false);
                    if unread_at_open > 0 {
                        new_from.set(first_unread(&new_rows, unread_at_open));
                        open_at_new.set_value(saved.is_none());
                    }
                } else {
                    if !pinned.get_value() {
                        let n = rows.with_untracked(|old| arrivals(old, &new_rows));
                        arrived.update(|a| *a += n);
                    }
                    // Screen readers hear arrivals, not every re-render of the list.
                    if let Some(text) = rows.with_untracked(|old| arrival_text(old, &new_rows)) {
                        announce.set(text);
                    }
                }
                let mine = new_rows
                    .iter()
                    .rev()
                    .find(|r| {
                        r.message.mine && r.message.state == SendState::Sent && !r.message.deleted
                    })
                    .map(|r| r.message.clone());
                c.last_mine.update_value(|l| match mine {
                    Some(m) => {
                        l.insert(list_key, m);
                    }
                    None => {
                        l.remove(&list_key);
                    }
                });
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

    let first_spec = spec(offset.get_value());
    leptos::task::spawn_local(async move {
        let r: Result<u64, String> = ipc::invoke_with_channel(
            "subscribe_window",
            json!({"spec": first_spec}),
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
                    row_el(&el, r.key)
                        .filter(|e| e.offset_top() as f64 + e.offset_height() as f64 > top)
                        .map(|e| (r.key, e.offset_top() as f64 - top))
                })
            });
            anchor.set_value(first);
        }
        offset.set_value(new_offset);
        offset_sig.set(new_offset);
        ipc::command(
            "move_window",
            json!({"accountId": account_id, "subId": id, "spec": spec(new_offset)}),
        );
    };

    // Back to the newest message, moving the window there first if needed.
    let to_end = move || {
        pinned.set_value(true);
        away.set(false);
        arrived.set(0);
        anchor.set_value(None);
        c.anchors.update_value(|a| {
            a.remove(&list_key);
        });
        if offset.get_value() > 0 {
            offset.set_value(0);
            offset_sig.set(0);
            if let Some(id) = sub_id.get_value() {
                ipc::command(
                    "move_window",
                    json!({"accountId": account_id, "subId": id, "spec": spec(0)}),
                );
            }
        } else if let Some(el) = scroller.get_untracked() {
            el.set_scroll_top(el.scroll_height());
            seen();
        }
    };

    // Sending re-pins the list it was sent to.
    Effect::new(move |_| {
        if c.repin.get() == Some(list_key) {
            c.repin.set(None);
            to_end();
        }
    });
    // Coming back to the window counts as reading what is on screen.
    Effect::new(move |_| {
        if c.active.get() {
            seen();
        }
    });

    // Remembers the row at the top of the list, for coming back to this conversation.
    let remember = move |el: &web_sys::HtmlDivElement| {
        if pinned.get_value() {
            c.anchors.update_value(|a| {
                a.remove(&list_key);
            });
            return;
        }
        let r = el.get_bounding_client_rect();
        let hit = document()
            .element_from_point((r.left() + 60.0) as f32, (r.top() + 8.0) as f32)
            .and_then(|e| e.closest("[data-key]").ok().flatten())
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
        if let Some(row) = hit
            && let Some(key) = row.get_attribute("data-key").and_then(|k| k.parse().ok())
        {
            let top = row.offset_top() as f64 - el.scroll_top() as f64;
            let off = offset.get_value();
            c.anchors.update_value(|a| {
                a.insert(
                    list_key,
                    crate::state::Anchor {
                        offset: off,
                        key,
                        top,
                    },
                );
            });
        }
    };

    let on_scroll = move |_| {
        let Some(el) = scroller.get_untracked() else {
            return;
        };
        let top = el.scroll_top();
        // Nothing to decide until rows are in and any remembered position is restored.
        if !loaded.get_untracked() || anchor.with_value(|a| a.is_some()) {
            last_top.set_value(top);
            return;
        }
        let bottom_gap = el.scroll_height() - top - el.client_height();
        let was = pinned.get_value();
        // Content growing under a pinned list fires scroll events too; only the reader
        // scrolling up (or a window away from the end) unpins it.
        if offset.get_value() > 0 {
            pinned.set_value(false);
        } else if bottom_gap < 40 {
            pinned.set_value(true);
        } else if top < last_top.get_value() - 1 {
            pinned.set_value(false);
        }
        last_top.set_value(top);
        let now = pinned.get_value();
        if now && !was {
            arrived.set(0);
            seen();
        }
        let far = !now && (bottom_gap > 240 || offset.get_value() > 0);
        if away.get_untracked() != far {
            away.set(far);
        }
        remember(&el);
        let loaded = rows.with_untracked(|r| r.len() as u32);
        let off = offset.get_value();
        if top < 200 && off + loaded < total.get_untracked() {
            move_window(off + STEP);
        } else if bottom_gap < 200 && off > 0 {
            move_window(off.saturating_sub(STEP));
        }
    };
    spike::register_scroller(scroller);

    // While pinned, any change in the rows' or the viewport's height keeps the newest row in view.
    let content: NodeRef<Div> = NodeRef::new();
    let observer = StoredValue::new_local(None::<ipc::ResizeObserver>);
    Effect::new(move |_| {
        let (Some(el), Some(inner)) = (scroller.get(), content.get()) else {
            return;
        };
        if observer.with_value(|o| o.is_some()) {
            return;
        }
        let target = el.clone();
        let ro = ipc::on_resize(&[&el, &inner], move || {
            if pinned.get_value() && anchor.with_value(|a| a.is_none()) {
                target.set_scroll_top(target.scroll_height());
            }
        });
        observer.set_value(Some(ro));
    });
    on_cleanup(move || {
        if let Some(Some(o)) = observer.try_update_value(|o| o.take()) {
            o.disconnect();
        }
    });

    // Day dividers sit above the first row of each day; a thread's root gets a reply count.
    let shown = move || {
        let rs = rows.get();
        let new_key = new_from.get();
        let mut prev: Option<i64> = None;
        let mut prev_row: Option<(Uuid, i64)> = None;
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
            let new_line = new_key == Some(r.key);
            let grouped = divider.is_none()
                && !new_line
                && !(thread_root_id.is_some() && i == 0)
                && prev_row
                    .is_some_and(|(author, t)| author == r.message.author_id && at - t < GROUP_MS);
            prev_row = Some((r.message.author_id, at));
            out.push((r, divider, new_line, grouped));
        }
        out
    };
    let replies = move || rows.with(|r| r.len().saturating_sub(1));
    // The intro block sits above the oldest message once the window reaches it.
    let at_start = move || {
        main && loaded.get() && {
            let n = rows.with(|r| r.len() as u32);
            n > 0 && offset_sig.get() + n >= total.get()
        }
    };
    let row_keys = move |ev: web_sys::KeyboardEvent| {
        let Some(row) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
            .filter(|t| t.class_list().contains("msg"))
        else {
            return;
        };
        // Rows are siblings, with day and New dividers between them.
        let is_row = |e: &web_sys::Element| e.class_list().contains("msg");
        let walk = |mut e: Option<web_sys::Element>, back: bool| {
            while let Some(x) = e {
                if is_row(&x) {
                    return Some(x);
                }
                e = if back {
                    x.previous_element_sibling()
                } else {
                    x.next_element_sibling()
                };
            }
            None
        };
        let parent = row.parent_element();
        let to = match ev.key().as_str() {
            "ArrowUp" => walk(row.previous_element_sibling(), true),
            "ArrowDown" => walk(row.next_element_sibling(), false),
            "Home" => walk(parent.as_ref().and_then(|p| p.first_element_child()), false),
            "End" => walk(parent.as_ref().and_then(|p| p.last_element_child()), true),
            _ => None,
        };
        if let Some(next) = to.and_then(|x| x.dyn_into::<web_sys::HtmlElement>().ok()) {
            ev.prevent_default();
            let _ = next.focus();
            next.scroll_into_view_with_bool(false);
        }
    };
    let pill = move || {
        let n = arrived.get();
        (away.get() || n > 0).then(|| {
            let text = if n > 0 {
                format!("{n} new · Jump to latest")
            } else {
                "Jump to latest".to_string()
            };
            let label = if n > 0 {
                format!(
                    "Jump to latest, {n} new {}",
                    if n == 1 { "message" } else { "messages" }
                )
            } else {
                "Jump to latest".to_string()
            };
            view! {
                <button class="jump" aria-label=label on:click=move |_| to_end()>
                    {sized(Icon::ArrowDown, 15)}{text}
                </button>
            }
        })
    };

    view! {
        <div class="listwrap">
            <div class="list" node_ref=scroller on:scroll=on_scroll role="region"
                 aria-label=label aria-busy=move || (!loaded.get()).to_string() on:keydown=row_keys>
                <div class="spacer"></div>
                <div class="lin" node_ref=content>
                {move || (!loaded.get()).then(skeleton)}
                {move || (loaded.get() && rows.with(|r| r.is_empty())).then(|| empty(account_id, conversation_id, thread_root_id))}
                {move || at_start().then(|| intro(account_id, conversation_id))}
                <For each=shown key=|(r, d, n, g)| (r.key, r.version, d.clone(), *n, *g) let:item>
                    {item.1.map(|d| if d.is_empty() {
                        view! { <div class="tsep">{move || format!("{} {}", replies(), if replies() == 1 { "reply" } else { "replies" })}</div> }.into_any()
                    } else {
                        view! { <div class="day" role="separator"><span>{d}</span></div> }.into_any()
                    })}
                    {item.2.then(|| view! { <div class="newdiv" role="separator" aria-label="New messages">"New"</div> })}
                    <Message account_id=account_id conversation_id=conversation_id row=item.0
                             in_thread=thread_root_id.is_some() grouped=item.3 roving=roving last_key=last_key />
                </For>
                </div>
            </div>
            {pill}
            <div class="visually-hidden" aria-live="polite">{move || announce.get()}</div>
        </div>
    }
}

/// An empty list. Away from the server, it says nothing is saved rather than that nothing exists.
fn empty(account_id: Uuid, conversation_id: Uuid, thread_root_id: Option<Uuid>) -> AnyView {
    let c = ctx();
    let connection = c.connection(account_id);
    if thread_root_id.is_some() {
        return view! { <p class="empty">"No replies yet."</p> }.into_any();
    }
    if connection == ConnectionState::Online {
        return intro(account_id, conversation_id);
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
    /// Same author as the row above, within five minutes: no avatar or name.
    grouped: bool,
    /// The list's one tab stop, moved by the arrow keys.
    roving: RwSignal<Option<Uuid>>,
    last_key: Memo<Option<Uuid>>,
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
                                    move |_| c.refill.set(Some(Refill { account_id, conversation_id, id, body: body.clone() }))
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
    let last_reply = m.last_reply_at_ms;
    let repliers = m.reply_user_ids.clone();
    let thread_summary = (!in_thread && replies > 0).then(|| {
        let faces = repliers
            .iter()
            .map(|u| avatar(*u, &c.name(account_id, *u), "s"))
            .collect_view();
        let label = format!(
            "{replies} {}, open thread",
            if replies == 1 { "reply" } else { "replies" }
        );
        view! {
            <button class="thr" aria-label=label on:click=move |_| c.aside.set(Aside::Thread(id))>
                <span class="faces">{faces}</span>
                <b>{format!("{replies} {}", if replies == 1 { "reply" } else { "replies" })}</b>
                {last_reply.map(|at| view! { <span>{move || format!("Last reply {}", ui::ago(at, c.now.get()))}</span> })}
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
    let (src, entities, edited, deleted) =
        (m.body.clone(), m.entities.clone(), m.edited, m.deleted);
    let editing_me = move || c.editing.with(|e| e.as_ref().is_some_and(|e| e.id == id));
    let body = move || {
        if deleted {
            None
        } else if editing_me() {
            Some(view! { <InlineEdit account_id=account_id conversation_id=conversation_id in_thread=in_thread /> }.into_any())
        } else {
            Some(view! { <LongBody src=src.clone() entities=entities.clone() reader=reader edited=edited /> }.into_any())
        }
    };
    let confirm = RwSignal::new(None::<(f64, f64)>);
    let delete = move || {
        confirm.set(None);
        ipc::command(
            "delete_message",
            json!({"accountId": account_id, "conversationId": conversation_id, "messageId": id}),
        );
    };
    let aria = if pending {
        "Your message, waiting to send".to_string()
    } else {
        format!("{} at {}", m.author_name, ui::clock(m.created_at_ms))
    };
    let author_id = m.author_id;
    let open_person = move |_| {
        c.overlay.set(crate::state::Overlay::Person {
            account_id,
            user_id: author_id,
        })
    };
    let tab_stop = move || roving.get().or(last_key.get()) == Some(id);

    view! {
        <div class=class class:cont=grouped class:pending=pending class:failed=failed data-key=id.to_string()
            role="article" aria-label=aria tabindex=move || if tab_stop() { "0" } else { "-1" }
            on:focus=move |_| roving.set(Some(id))>
            {if grouped {
                view! { <span class="gut" aria-hidden="true">{ui::short_clock(m.created_at_ms)}</span> }.into_any()
            } else {
                avatar(m.author_id, &m.author_name, "").into_any()
            }}
            <div class="mc">
                {(!grouped).then(|| view! {
                    <div class="mh">
                        <button class="name" class:dim=m.deleted on:click=open_person>{author}</button>
                        <span class="time">{ui::clock(m.created_at_ms)}</span>
                    </div>
                })}
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
                            <button class="iconbtn" aria-label="Edit message" on:click=move |_| c.edit(target.clone())>{icon(Icon::Edit)}</button>
                            <button class="iconbtn" aria-label="Delete message" aria-haspopup="dialog"
                                on:click=move |ev| confirm.set(Some(anchor_of(&ev)))>{icon(Icon::Trash)}</button>
                        }
                    })}
                </div>
            })}
            {move || picking.get().map(|at| view! {
                <EmojiPicker account_id=account_id at=at on_pick=Callback::new(react)
                    on_close=Callback::new(move |_| picking.set(None)) />
            })}
            {move || confirm.get().map(|at| view! {
                <ConfirmDelete at=at on_delete=Callback::new(move |_| delete())
                    on_close=Callback::new(move |_| confirm.set(None)) />
            })}
        </div>
    }
}

/// A message body that collapses past about twelve lines, with Show more.
#[component]
fn LongBody(src: String, entities: Vec<Entity>, reader: Reader, edited: bool) -> impl IntoView {
    let el: NodeRef<Div> = NodeRef::new();
    let overflows = RwSignal::new(false);
    let open = RwSignal::new(false);
    let watch = StoredValue::new_local(None::<ipc::SizeWatch>);
    // Open, the body isn't clamped and can't be measured; Show less stays.
    let measure = move || {
        if let Some(d) = el.try_get_untracked().flatten()
            && open.try_get_untracked() == Some(false)
        {
            let over = d.scroll_height() > d.client_height() + 2;
            if overflows.try_get_untracked() != Some(over) {
                overflows.try_set(over);
            }
        }
    };
    Effect::new(move |_| {
        if el.get().is_some() {
            ipc::next_frame(move || {
                measure();
                // A narrower column wraps a long enough body past the clamp: measure again.
                if let Some(d) = el.try_get_untracked().flatten()
                    && may_clamp_when_narrower(d.scroll_height() as f64)
                    && watch.try_with_value(|w| w.is_none()) == Some(true)
                {
                    watch.set_value(Some(ipc::watch_size(&d, measure)));
                }
            });
        }
    });
    on_cleanup(move || drop(watch.try_update_value(|w| w.take())));
    view! {
        <div class="body" class:clamp=move || !open.get() class:faded=move || overflows.get() && !open.get()
            node_ref=el>{markdown::body(&src, &entities, reader, edited)}</div>
        {move || overflows.get().then(|| view! {
            <button class="more" aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)>
                {move || if open.get() { "Show less" } else { "Show more" }}
            </button>
        })}
    }
}

/// The height of `.body.clamp` in style.css: 18em at 14.5px, twelve lines.
const CLAMP_PX: f64 = 18.0 * 14.5;

/// Whether a body this tall could pass the clamp once its column narrows. Below three
/// lines it would need a column a quarter as wide, narrower than the window allows, so
/// short messages, most of a list, are never watched.
fn may_clamp_when_narrower(content_px: f64) -> bool {
    content_px * 4.0 >= CLAMP_PX
}

/// Asks before deleting a message, in a small popover at the trash button.
#[component]
fn ConfirmDelete(at: (f64, f64), on_delete: Callback<()>, on_close: Callback<()>) -> impl IntoView {
    // Return on open must not delete: Cancel takes focus, as when removing an account.
    let cancel: NodeRef<leptos::html::Button> = NodeRef::new();
    let pop: NodeRef<Div> = NodeRef::new();
    let opener = StoredValue::new_local(document().active_element());
    Effect::new(move |_| {
        if let Some(b) = cancel.get() {
            let _ = b.focus();
        }
    });
    // It's placed for the window it opened in; a resize could leave it off screen.
    let resized = window_event_listener(leptos::ev::resize, move |_| on_close.run(()));
    on_cleanup(move || {
        resized.remove();
        ui::give_back_focus(
            opener.try_update_value(|o| o.take()).flatten(),
            pop.get_untracked().map(Into::into),
        );
    });
    let h = web_sys::window()
        .and_then(|w| w.inner_height().ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(800.0);
    let (x, y) = at;
    let top = if y + 130.0 > h {
        y - 34.0 - 120.0
    } else {
        y + 6.0
    };
    let style = format!("left: {}px; top: {}px", (x - 264.0).max(8.0), top.max(8.0));
    view! {
        <div class="backdrop" on:click=move |_| on_close.run(())></div>
        <div class="confirm" style=style role="alertdialog" aria-labelledby="del-t" aria-describedby="del-d" node_ref=pop
            on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Escape" { ev.prevent_default(); on_close.run(()) }>
            <p id="del-t" class="ctitle">"Delete this message?"</p>
            <p id="del-d" class="muted">"It's removed for everyone in the conversation. This can't be undone."</p>
            <div class="crow">
                <button type="button" class="btn" node_ref=cancel on:click=move |_| on_close.run(())>"Cancel"</button>
                <button type="button" class="btn dangerfill" on:click=move |_| on_delete.run(())>"Delete"</button>
            </div>
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
    if let Some(el) = textarea(r) {
        let _ = el.focus();
        let _ = el.set_selection_range(ui::byte_to_utf16(&t, from), ui::byte_to_utf16(&t, to));
        if layer::insert_text(insert) {
            // The field's input event has updated `text`.
            if caret_back > 0 {
                let after = text.get_untracked();
                let pos = ui::byte_to_utf16(&after, from + insert.len() - caret_back);
                let _ = el.set_selection_range(pos, pos);
            }
            return;
        }
    }
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

/// Text before the caret, for the fence rule.
fn before_caret(text: RwSignal<String>, r: NodeRef<Textarea>) -> String {
    let t = text.get_untracked();
    let (s, _) = selection(&t, r);
    t[..s].to_string()
}

/// Wraps the selection in `marks`, or puts the caret between a pair of them.
fn wrap(text: RwSignal<String>, r: NodeRef<Textarea>, before: &str, after: &str) {
    let t = text.get_untracked();
    let (s, e) = selection(&t, r);
    let inner = &t[s..e];
    splice(
        text,
        r,
        s,
        e,
        &format!("{before}{inner}{after}"),
        after.len(),
    );
}

/// ⌘B, ⌘I, ⌘E and ⌘⇧X. Returns whether the key was a formatting shortcut.
fn format_shortcut(
    ev: &web_sys::KeyboardEvent,
    text: RwSignal<String>,
    r: NodeRef<Textarea>,
) -> bool {
    if !ev.meta_key() || ev.alt_key() || ev.ctrl_key() {
        return false;
    }
    match compose::shortcut_markers(&ev.key(), ev.shift_key()) {
        Some(m) => {
            ev.prevent_default();
            wrap(text, r, m, m);
            true
        }
        None => false,
    }
}

/// The DOM id of a conversation's or thread's composer.
pub fn composer_id(conversation_id: Uuid, thread: bool) -> String {
    format!("composer-{conversation_id}-{thread}")
}

pub fn focus_composer(conversation_id: Uuid, thread: bool) {
    let id = composer_id(conversation_id, thread);
    ipc::next_frame(move || {
        if let Some(el) = document()
            .get_element_by_id(&id)
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = el.focus();
        }
    });
}

/// Keeps a composer's text in the core on a short debounce, and once more as it closes.
fn keep_draft(
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
    text: RwSignal<String>,
    ready: RwSignal<bool>,
) {
    let save = move |t: String| {
        ipc::command(
            "save_composer_draft",
            json!({"accountId": account_id, "conversationId": conversation_id,
                   "threadRootId": thread_root_id, "text": t}),
        )
    };
    // Plain shared cells: the cleanup below runs after this component's signals are gone.
    let pending = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let generation = StoredValue::new(0u32);
    let p = pending.clone();
    Effect::new(move |_| {
        let t = text.get();
        if !ready.get() {
            return;
        }
        if let Ok(mut slot) = p.lock() {
            *slot = Some(t);
        }
        let n = generation.get_value() + 1;
        generation.set_value(n);
        let p = p.clone();
        set_timeout(
            move || {
                if generation.try_get_value() != Some(n) {
                    return;
                }
                if let Some(t) = p.lock().ok().and_then(|mut s| s.take()) {
                    save(t);
                }
            },
            std::time::Duration::from_millis(400),
        );
    });
    on_cleanup(move || {
        if let Some(t) = pending.lock().ok().and_then(|mut s| s.take()) {
            save(t);
        }
    });
}

/// Stages dropped files in the composer under the drop point, or else the open conversation's.
pub fn claim_drop(c: crate::state::Ctx, drop_id: u64, x: f64, y: f64) {
    let at = document()
        .element_from_point(x as f32, y as f32)
        .and_then(|e| e.closest(".comp[data-conv]").ok().flatten())
        .and_then(|el| {
            let id = |name: &str| {
                el.get_attribute(name)
                    .and_then(|v| Uuid::parse_str(&v).ok())
            };
            Some((id("data-account")?, id("data-conv")?, id("data-thread")))
        });
    let Some(key) = at.or_else(|| {
        c.selected
            .get_untracked()
            .map(|s| (s.account_id, s.conversation_id, None))
    }) else {
        return;
    };
    leptos::task::spawn_local(async move {
        let r: Result<usize, String> = ipc::invoke(
            "stage_dropped",
            json!({"accountId": key.0, "conversationId": key.1, "threadRootId": key.2,
                   "dropId": drop_id}),
        )
        .await;
        if let Err(e) = r {
            c.stage_error.set(Some((key, e)));
        }
    });
}

/// Which autocomplete a typed sigil opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sigil {
    At,
    Colon,
    Hash,
}

/// Autocomplete suggestions for what is being typed.
#[derive(Debug, Clone, PartialEq)]
enum Hits {
    None,
    People(Vec<Member>),
    Emoji(Vec<emoji::Found>),
    Channels(Vec<(Uuid, String)>),
}

impl Hits {
    fn len(&self) -> usize {
        match self {
            Hits::None => 0,
            Hits::People(v) => v.len(),
            Hits::Emoji(v) => v.len(),
            Hits::Channels(v) => v.len(),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Hits::Emoji(_) => "Emoji",
            Hits::Channels(_) => "Channels",
            _ => "People to mention",
        }
    }
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
    let draft_ready = RwSignal::new(false);
    let picked = StoredValue::new(Vec::<(compose::Pick, String)>::new());
    let area: NodeRef<Textarea> = NodeRef::new();
    let hl: NodeRef<Div> = NodeRef::new();
    let boxes: NodeRef<Div> = NodeRef::new();
    let last_typing = StoredValue::new(0f64);
    let members = RwSignal::new(Vec::<Member>::new());
    let ac = RwSignal::new(None::<(Sigil, usize, String)>);
    let ac_index = RwSignal::new(0usize);
    let emoji = RwSignal::new(false);
    let fence = RwSignal::new(false);
    let me = c.me(account_id).unwrap_or_default();
    let offline = move || c.connection(account_id) != ConnectionState::Online;
    let thread = thread_root_id.is_some();
    let key = (account_id, conversation_id, thread_root_id);
    let notice = RwSignal::new(None::<String>);
    let staged = move || c.staged.with(|m| m.get(&key).cloned().unwrap_or_default());
    // Files staged before this composer opened (another conversation's visit, a reload).
    leptos::task::spawn_local(async move {
        let args = json!({"accountId": account_id, "conversationId": conversation_id,
                          "threadRootId": thread_root_id});
        if let Ok(files) = ipc::invoke::<Vec<StagedFile>>("staged_files", args).await
            && !files.is_empty()
        {
            c.staged.update(|m| {
                m.entry(key).or_insert(files);
            });
        }
    });
    Effect::new(move |_| {
        if let Some((k, e)) = c.stage_error.get()
            && k == key
        {
            c.stage_error.set(None);
            notice.set(Some(format!("Could not attach: {e}")));
        }
    });
    // A notice that Send had to wait goes once nothing blocks it.
    Effect::new(move |_| {
        if compose::send_blocked(&staged()).is_none()
            && notice.with_untracked(|n| {
                n.as_deref()
                    .is_some_and(|n| n.starts_with("Wait") || n.starts_with("Remove or retry"))
            })
        {
            notice.set(None);
        }
    });

    keep_draft(
        account_id,
        conversation_id,
        thread_root_id,
        text,
        draft_ready,
    );
    // The field's value is written only when it differs, so typing and IME composition are
    // never interrupted and the field keeps its undo stack.
    Effect::new(move |_| {
        let t = text.get();
        if let Some(el) = area.get()
            && el.value() != t
        {
            el.set_value(&t);
        }
        queue_microtask(move || {
            layer::sync(area, hl);
            layer::shade(hl, boxes);
        });
    });
    Effect::new(move |_| {
        if let Some(el) = area.get() {
            let ro = ipc::on_resize(&[&el], move || {
                layer::sync(area, hl);
                layer::shade(hl, boxes);
            });
            on_cleanup(move || ro.disconnect());
        }
    });
    let refresh_fence = move || {
        let t = text.get_untracked();
        let (caret, _) = selection(&t, area);
        fence.set(compose::in_open_fence(&t[..caret]));
    };
    leptos::task::spawn_local(async move {
        let args = json!({"accountId": account_id, "conversationId": conversation_id,
                          "threadRootId": thread_root_id});
        let saved = ipc::invoke::<Option<String>>("composer_draft", args).await;
        if let Ok(Some(t)) = saved
            && text.with_untracked(|x| x.is_empty())
        {
            text.set(t);
            ipc::next_frame(refresh_fence);
        }
        draft_ready.set(true);
    });

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

    // The composer discards the queued send only once its text has somewhere to go.
    Effect::new(move |_| {
        if let Some(r) = c.refill.get()
            && r.conversation_id == conversation_id
            && !thread
        {
            c.refill.set(None);
            if compose::can_refill(&text.get_untracked(), draft_ready.get_untracked()) {
                ipc::command(
                    "discard_send",
                    json!({"accountId": r.account_id, "id": r.id}),
                );
                text.set(r.body);
                notice.set(None);
            } else {
                notice.set(Some(compose::REFILL_REFUSED.to_string()));
            }
        }
    });
    Effect::new(move |_| {
        if text.with(|t| t.trim().is_empty())
            && notice.with_untracked(|n| n.as_deref() == Some(compose::REFILL_REFUSED))
        {
            notice.set(None);
        }
    });

    // Opening a conversation puts the caret here, unless the sidebar keyboard opened it.
    let keyboard = c.keyboard_open.get_value();
    c.keyboard_open.set_value(false);
    if thread || !keyboard {
        focus_composer(conversation_id, thread);
    }

    // `#` offers this workspace's channels that the user is in.
    let channels = Memo::new(move |_| {
        c.conversation(crate::state::Selection {
            account_id,
            conversation_id,
        })
        .map(|(w, _)| {
            w.conversations
                .into_iter()
                .filter(|v| matches!(v.kind, ConversationKind::Public | ConversationKind::Private))
                .map(|v| (v.id, v.title.trim_start_matches('#').to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    });
    let hits = Memo::new(move |_| match ac.get() {
        None => Hits::None,
        Some((Sigil::At, _, q)) => Hits::People(members.with(|ms| {
            ms.iter()
                .filter(|m| m.user_id != me && compose::matches_query(&m.display_name, &q))
                .take(6)
                .cloned()
                .collect()
        })),
        Some((Sigil::Colon, _, q)) => Hits::Emoji(emoji::search(&q, 6)),
        Some((Sigil::Hash, _, q)) => Hits::Channels(channels.with(|cs| {
            compose::channel_matches(cs, &q, 6)
                .into_iter()
                .cloned()
                .collect()
        })),
    });
    let ac_open = move || hits.with(|h| h.len() > 0);

    let refresh = move || {
        let t = text.get_untracked();
        let (caret, end) = selection(&t, area);
        fence.set(compose::in_open_fence(&t[..caret]));
        let q = (caret == end)
            .then(|| {
                compose::mention_query(&t, caret)
                    .map(|(at, q)| (Sigil::At, at, q))
                    .or_else(|| {
                        compose::channel_query(&t, caret).map(|(at, q)| (Sigil::Hash, at, q))
                    })
                    .or_else(|| {
                        compose::emoji_query(&t, caret).map(|(at, q)| (Sigil::Colon, at, q))
                    })
            })
            .flatten();
        if q.is_none() || q != ac.get_untracked() {
            ac_index.set(0);
        }
        ac.set(q);
    };

    // Replaces the typed `@name` or `:name` with the suggestion at `i`.
    let pick = move |i: usize| {
        let Some((_, at, _)) = ac.get_untracked() else {
            return;
        };
        let t = text.get_untracked();
        let (caret, _) = selection(&t, area);
        let insert = match hits.get_untracked() {
            Hits::None => return,
            Hits::People(v) => {
                let Some(m) = v.get(i) else { return };
                let name = format!("@{}", m.display_name);
                picked.update_value(|p| p.push((compose::Pick::User(m.user_id), name.clone())));
                format!("{name} ")
            }
            Hits::Channels(v) => {
                let Some((id, name)) = v.get(i) else { return };
                let name = format!("#{name}");
                picked.update_value(|p| p.push((compose::Pick::Channel(*id), name.clone())));
                format!("{name} ")
            }
            Hits::Emoji(v) => {
                let Some(f) = v.get(i) else { return };
                f.emoji.to_string()
            }
        };
        ac.set(None);
        splice(text, area, at, caret, &insert, 0);
    };

    // Text and staged files go out as one message, once every file has uploaded.
    let send = move || {
        let body = text.get_untracked();
        let files = staged();
        if body.trim().is_empty() && files.is_empty() {
            return;
        }
        if let Some(why) = compose::send_blocked(&files) {
            notice.set(Some(why.to_string()));
            return;
        }
        let mentions = picked.with_value(|p| compose::picked_entities(&body, p));
        text.set(String::new());
        fence.set(false);
        picked.set_value(vec![]);
        ac.set(None);
        notice.set(None);
        c.repin.set(Some((conversation_id, thread_root_id)));
        leptos::task::spawn_local(async move {
            let sent: Result<Uuid, String> = ipc::invoke(
                "send_message",
                json!({"accountId": account_id, "conversationId": conversation_id,
                       "body": body, "threadRootId": thread_root_id, "mentions": mentions}),
            )
            .await;
            if let Err(e) = sent {
                notice.set(Some(e));
                if text.with_untracked(String::is_empty) {
                    text.set(body);
                }
            }
        });
    };

    let list = move |marker: &'static str| {
        let t = text.get_untracked();
        let (s, _) = selection(&t, area);
        let line = t[..s].rfind('\n').map_or(0, |i| i + 1);
        splice(text, area, line, line, marker, 0);
    };
    let block = move || {
        let t = text.get_untracked();
        let (s, e) = selection(&t, area);
        let start = s == 0 || t[..s].ends_with('\n');
        let (insert, back) = compose::code_block(&t[s..e], start);
        splice(text, area, s, e, &insert, back);
        ipc::next_frame(refresh);
    };

    let attach = move |_| {
        notice.set(None);
        leptos::task::spawn_local(async move {
            let r: Result<usize, String> = ipc::invoke(
                "pick_files",
                json!({"accountId": account_id, "conversationId": conversation_id, "threadRootId": thread_root_id}),
            )
            .await;
            if let Err(e) = r {
                notice.set(Some(format!("Could not attach: {e}")));
            }
        });
    };
    // Pasted files (a screenshot, or files copied in Finder) are staged; pasted text,
    // HTML included, goes into the field as plain text, as the field does on its own.
    let paste = move |ev: web_sys::ClipboardEvent| {
        let Some(dt) = ev.clipboard_data() else {
            return;
        };
        let Some(list) = dt.files() else { return };
        let files: Vec<web_sys::File> = (0..list.length()).filter_map(|i| list.item(i)).collect();
        let names: Vec<String> = files.iter().map(web_sys::File::name).collect();
        let pasted_text = dt.get_data("text/plain").unwrap_or_default();
        if !compose::paste_stages_files(&pasted_text, &names) {
            return;
        }
        ev.prevent_default();
        notice.set(None);
        for f in files {
            leptos::task::spawn_local(async move {
                let staged = async {
                    let buf = wasm_bindgen_futures::JsFuture::from(f.array_buffer())
                        .await
                        .map_err(|_| "could not read the pasted file".to_string())?;
                    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                    let name = String::from(js_sys::encode_uri_component(&f.name()));
                    let headers = [
                        ("x-account", account_id.to_string()),
                        ("x-conversation", conversation_id.to_string()),
                        (
                            "x-thread",
                            thread_root_id.map(|t| t.to_string()).unwrap_or_default(),
                        ),
                        ("x-name", name),
                    ];
                    ipc::invoke_bytes::<Uuid>("stage_pasted", &bytes, &headers).await
                };
                if let Err(e) = staged.await {
                    notice.set(Some(format!("Could not attach: {e}")));
                }
            });
        }
    };
    let rows =
        move || text.with(|t| t.lines().count().clamp(1, 8) + usize::from(t.ends_with('\n')));
    let id = composer_id(conversation_id, thread);
    let list_id = StoredValue::new(format!("{id}-ac"));
    let placeholder2 = placeholder.clone();
    let fmt = |label: &'static str, i: Icon, marks: &'static str| {
        view! {
            <button type="button" class="iconbtn" aria-label=label title=label
                on:click=move |_| wrap(text, area, marks, marks)>{icon(i)}</button>
        }
    };

    let chip = move |f: StagedFile| {
        let id = f.id;
        let name = f.name.clone();
        let failed = matches!(f.state, StagedState::Failed { .. });
        let uploading = matches!(f.state, StagedState::Uploading);
        let pct = (f.sent * 100).checked_div(f.size).unwrap_or(0);
        let status = compose::chip_status(&f, |n| ui::size(n as i64));
        let args = move || {
            json!({"accountId": account_id, "conversationId": conversation_id,
                                  "threadRootId": thread_root_id, "id": id})
        };
        view! {
            <li class="fchip" class:failed=failed aria-busy=uploading.to_string()>
                <span class="fic">{icon(Icon::File)}</span>
                <span class="fmeta">
                    <span class="fn" title=name.clone()>{name.clone()}</span>
                    <span class="fs" role=failed.then_some("alert")>{status}</span>
                </span>
                {failed.then(|| view! {
                    <button type="button" class="iconbtn sm" aria-label=format!("Retry {name}") title="Retry"
                        on:click=move |_| ipc::command("retry_staged", args())>{sized(Icon::Refresh, 14)}</button>
                })}
                <button type="button" class="iconbtn sm" aria-label=format!("Remove {}", f.name) title="Remove"
                    on:click=move |_| ipc::command("remove_staged", args())>{sized(Icon::Close, 14)}</button>
                {uploading.then(|| view! { <span class="bar" style=format!("width: {pct}%")></span> })}
            </li>
        }
    };

    view! {
        <div class="comp" class:dropping=move || c.dragging.get()
            data-account=account_id.to_string() data-conv=conversation_id.to_string()
            data-thread=thread_root_id.map(|t| t.to_string()).unwrap_or_default()>
            <Show when=move || !staged().is_empty()>
                <ul class="chips" aria-label="Files to send">
                    {move || staged().into_iter().map(chip).collect_view()}
                </ul>
            </Show>
            <div class="cwrap">
                <div class="hlayer" aria-hidden="true" node_ref=hl>
                    <div class="hcbox" node_ref=boxes></div>
                    {move || text.with(|t| picked.with_value(|p| layer::content(t, &compose::picked_entities(t, p))))}
                </div>
                <label class="visually-hidden" for=id.clone()>{label.clone()}</label>
                <textarea id=id class="ctext over" node_ref=area rows=rows
                    placeholder=move || match c.connection(account_id) {
                        ConnectionState::Online => placeholder2.clone(),
                        ConnectionState::SignedOut => format!("{placeholder2}. Sends after you sign in"),
                        _ => format!("{placeholder2}. Sends when you're back online"),
                    }
                    aria-autocomplete="list" aria-expanded=move || ac_open().to_string()
                    aria-controls=list_id.get_value()
                    aria-activedescendant=move || ac_open().then(|| format!("{}-{}", list_id.get_value(), ac_index.get()))
                    on:scroll=move |_| layer::sync(area, hl)
                    on:paste=paste
                    on:blur=move |_| ac.set(None)
                    on:input=move |ev| {
                        text.set(event_target_value(&ev));
                        refresh();
                        let now = ipc::now_ms();
                        if now - last_typing.get_value() > 3000.0 {
                            last_typing.set_value(now);
                            ipc::command("typing", json!({"accountId": account_id, "conversationId": conversation_id}));
                        }
                    }
                    on:click=move |_| refresh()
                    on:keyup=move |ev: web_sys::KeyboardEvent| if ev.key().starts_with("Arrow") { refresh() }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.is_composing() {
                            return;
                        }
                        let n = hits.with_untracked(Hits::len);
                        if n > 0 {
                            match ev.key().as_str() {
                                "ArrowDown" => { ev.prevent_default(); ac_index.update(|i| *i = (*i + 1) % n); return; }
                                "ArrowUp" => { ev.prevent_default(); ac_index.update(|i| *i = (*i + n - 1) % n); return; }
                                "Enter" | "Tab" => { ev.prevent_default(); pick(ac_index.get_untracked().min(n - 1)); return; }
                                "Escape" => { ev.prevent_default(); ac.set(None); return; }
                                _ => {}
                            }
                        }
                        if format_shortcut(&ev, text, area) {
                            return;
                        }
                        match ev.key().as_str() {
                            "Enter" => {
                                if compose::on_return(ev.meta_key(), ev.shift_key(), &before_caret(text, area)) == compose::Return::Send {
                                    ev.prevent_default();
                                    send();
                                }
                            }
                            "ArrowUp" if !ev.shift_key() && !ev.meta_key() && !ev.alt_key()
                                && text.with_untracked(|t| t.is_empty()) => {
                                if let Some(m) = c.last_mine.with_value(|l| l.get(&(conversation_id, thread_root_id)).cloned()) {
                                    ev.prevent_default();
                                    c.edit(m);
                                }
                            }
                            _ => {}
                        }
                    }
                ></textarea>
                <Show when=ac_open>
                    <ul class="ac" role="listbox" id=list_id.get_value() aria-label=move || hits.with(Hits::label)>
                        {move || {
                            let lid = list_id.get_value();
                            let option = move |i: usize, body: AnyView| view! {
                                <li role="option" id=format!("{lid}-{i}") aria-selected=move || (ac_index.get() == i).to_string()
                                    class:on=move || ac_index.get() == i
                                    on:mousedown=move |ev| { ev.prevent_default(); pick(i); }>{body}</li>
                            };
                            match hits.get() {
                                Hits::None => vec![],
                                Hits::People(v) => v.into_iter().enumerate().map(|(i, m)| {
                                    let online = c.online(account_id, m.user_id);
                                    option(i, view! {
                                        {avatar(m.user_id, &m.display_name, "s")}
                                        <span class="acname">{m.display_name.clone()}</span>
                                        <span class="pres" class:online=online aria-label=if online { "online" } else { "offline" }></span>
                                    }.into_any())
                                }).collect::<Vec<_>>(),
                                Hits::Channels(v) => v.into_iter().enumerate().map(|(i, (_, name))| {
                                    option(i, view! {
                                        <span class="acemo" aria-hidden="true">{icon(Icon::Hash)}</span>
                                        <span class="acname">{name}</span>
                                    }.into_any())
                                }).collect::<Vec<_>>(),
                                Hits::Emoji(v) => v.into_iter().enumerate().map(|(i, f)| {
                                    option(i, view! {
                                        <span class="acemo" aria-hidden="true">{f.emoji}</span>
                                        <span class="acname">{f.label}</span>
                                        {f.hint.map(|h| view! { <span class="acsub">{h}</span> })}
                                    }.into_any())
                                }).collect::<Vec<_>>(),
                            }
                        }}
                    </ul>
                </Show>
            </div>
            <div class="cfoot" role="toolbar" aria-label="Formatting and attachments">
                <span class="cgroup">
                    {fmt("Bold, Command B", Icon::Bold, "**")}
                    {fmt("Italic, Command I", Icon::Italic, "_")}
                    {fmt("Strikethrough, Command Shift X", Icon::Strike, "~~")}
                    {fmt("Inline code, Command E", Icon::Code, "`")}
                    <button type="button" class="iconbtn" aria-label="Code block" title="Code block"
                        aria-pressed=move || fence.get().to_string() on:click=move |_| block()>{icon(Icon::CodeBlock)}</button>
                </span>
                <span class="vsep cgroup2"></span>
                <span class="cgroup2">
                    <button type="button" class="iconbtn" aria-label="Link" title="Link" on:click=move |_| wrap(text, area, "[", "](https://)")>{icon(Icon::Link)}</button>
                    <button type="button" class="iconbtn" aria-label="Bulleted list" title="Bulleted list" on:click=move |_| list("- ")>{icon(Icon::List)}</button>
                    <button type="button" class="iconbtn" aria-label="Numbered list" title="Numbered list" on:click=move |_| list("1. ")>{icon(Icon::NumberedList)}</button>
                </span>
                <span class="vsep"></span>
                <button type="button" class="iconbtn" on:click=attach aria-label="Attach files" title="Attach files">{icon(Icon::Attach)}</button>
                <button type="button" class="iconbtn" aria-label="Mention someone" title="Mention someone" on:click=move |_| {
                    let t = text.get_untracked();
                    let (s, e) = selection(&t, area);
                    let lead = if s == 0 || t[..s].ends_with(char::is_whitespace) { "@" } else { " @" };
                    splice(text, area, s, e, lead, 0);
                    ipc::next_frame(refresh);
                }>{icon(Icon::At)}</button>
                <span class="emowrap">
                    <button type="button" class="iconbtn" aria-label="Emoji" title="Emoji" on:click=move |_| emoji.set(true)>{icon(Icon::Smile)}</button>
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
                <span class="hint">{move || if c.dragging.get() {
                        view! { "Drop to attach" }.into_any()
                    } else if staged().iter().any(|f| matches!(f.state, StagedState::Uploading)) {
                        view! { "Uploading · Send when it's done" }.into_any()
                    } else if c.connection(account_id) == ConnectionState::SignedOut {
                        view! { "Signed out · messages wait in your outbox" }.into_any()
                    } else if offline() {
                        view! { "Offline · messages wait in your outbox" }.into_any()
                    } else if ac_open() {
                        let what = hits.with(|h| match h {
                            Hits::Emoji(v) => v.get(ac_index.get()).map_or(String::new(), |f| format!(" {}", f.emoji)),
                            _ => String::new(),
                        });
                        view! { "↑↓ choose · Tab or Return inserts"{what}" · Esc closes" }.into_any()
                    } else if fence.get() {
                        view! { <b>"Code block"</b>" · Return adds a line · ⌘Return sends" }.into_any()
                    } else {
                        view! { "Return to send · ⇧Return new line" }.into_any()
                    }}</span>
                <button type="button" class="sendbtn" aria-label=move || if offline() { "Queue message" } else { "Send" }
                    on:click=move |_| send()>{icon(Icon::Send)}</button>
            </div>
        </div>
        {move || notice.get().map(|e| view! { <p class="error attach-error" role="alert">{e}</p> })}
    }
}

/// One of my messages being edited in its row.
#[component]
fn InlineEdit(account_id: Uuid, conversation_id: Uuid, in_thread: bool) -> impl IntoView {
    let c = ctx();
    let area: NodeRef<Textarea> = NodeRef::new();
    let text = c.edit_text;
    let close = move || {
        c.editing.set(None);
        focus_composer(conversation_id, in_thread);
    };
    let save = move || {
        let body = text.get_untracked();
        if let Some(m) = c.editing.get_untracked()
            && !body.trim().is_empty()
            && body != m.body
        {
            ipc::command(
                "edit_message",
                json!({"accountId": account_id, "conversationId": conversation_id,
                       "messageId": m.id, "baseRevision": m.revision, "body": body}),
            );
        }
        close();
    };
    Effect::new(move |_| {
        if let Some(el) = area.get() {
            let n = el.value().encode_utf16().count() as u32;
            let _ = el.focus();
            let _ = el.set_selection_range(n, n);
        }
    });
    let rows =
        move || text.with(|t| t.lines().count().clamp(1, 12) + usize::from(t.ends_with('\n')));
    view! {
        <div class="comp inedit">
            <label class="visually-hidden" for="edit-box">"Edit message"</label>
            <textarea id="edit-box" class="ctext" node_ref=area rows=rows prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.is_composing() || format_shortcut(&ev, text, area) {
                        return;
                    }
                    match ev.key().as_str() {
                        "Enter" if compose::on_return(ev.meta_key(), ev.shift_key(), &before_caret(text, area)) == compose::Return::Send => {
                            ev.prevent_default();
                            save();
                        }
                        "Escape" => { ev.prevent_default(); close(); }
                        _ => {}
                    }
                }></textarea>
            <div class="cfoot">
                <span class="hint left">"Escape to cancel · Return to save"</span>
                <button type="button" class="btn" on:click=move |_| close()>"Cancel"</button>
                <button type="button" class="btn primary" on:click=move |_| save()>"Save"</button>
            </div>
        </div>
    }
}
