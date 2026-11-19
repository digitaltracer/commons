//! ⌘K: jump to any conversation, person or channel across accounts, or search messages.
use leptos::html::Input;
use leptos::prelude::*;
use proto::model::{Conversation, ConversationKind, Member};
use proto::view::ConnectionState;
use serde_json::json;
use uuid::Uuid;

use crate::ipc;
use crate::panels::Dialog;
use crate::state::{Mode, Overlay, Selection, ctx, is_dm};
use crate::ui::{Icon, avatar, icon, sized};

/// How well `name` matches `q`: prefix beats word start beats substring beats letters in
/// order. Also returns the matched run, for bolding, when the match is contiguous.
pub fn score(name: &str, q: &str) -> Option<(i32, Option<(usize, usize)>)> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Some((0, None));
    }
    let n = name.to_lowercase();
    // Lower-casing can change byte lengths; only report ranges when it didn't.
    let same = n.len() == name.len();
    let range = |at: usize| same.then_some((at, at + q.len()));
    if n.starts_with(&q) {
        return Some((1000 - n.len() as i32, range(0)));
    }
    if let Some(at) = n
        .match_indices(&q)
        .map(|(i, _)| i)
        .find(|i| n[..*i].ends_with(|c: char| !c.is_alphanumeric()))
    {
        return Some((800 - n.len() as i32, range(at)));
    }
    if let Some(at) = n.find(&q) {
        return Some((600 - n.len() as i32, range(at)));
    }
    // Every letter in order, fewer gaps scoring higher.
    let mut gaps = 0;
    let mut chars = n.chars().peekable();
    for qc in q.chars() {
        loop {
            match chars.next() {
                Some(c) if c == qc => break,
                Some(_) => gaps += 1,
                None => return None,
            }
        }
    }
    Some((300 - gaps - n.len() as i32, None))
}

#[derive(Clone, PartialEq)]
enum Target {
    Open(Selection),
    Dm {
        account_id: Uuid,
        workspace_id: Uuid,
        user_id: Uuid,
    },
    Join {
        account_id: Uuid,
        conversation_id: Uuid,
    },
    Search(String),
}

#[derive(Clone, PartialEq)]
struct Item {
    target: Target,
    label: String,
    glyph: Glyph,
    sub: String,
    section: &'static str,
    rank: i32,
    hit: Option<(usize, usize)>,
}

#[derive(Clone, PartialEq)]
enum Glyph {
    Icon(Icon),
    Person(Uuid),
    Group(usize),
}

/// People and unjoined channels for one workspace, fetched when the palette opens.
#[derive(Clone)]
struct Extra {
    account_id: Uuid,
    workspace_id: Uuid,
    workspace: String,
    members: Vec<Member>,
    channels: Vec<Conversation>,
}

fn glyph_view(g: &Glyph, label: &str) -> AnyView {
    match g {
        Glyph::Icon(i) => icon(*i).into_any(),
        Glyph::Person(id) => avatar(*id, label, "s").into_any(),
        Glyph::Group(n) => view! {
            <span class="av s" style="background: #6B6F6D" aria-hidden="true">{*n}</span>
        }
        .into_any(),
    }
}

fn label_view(label: &str, hit: Option<(usize, usize)>) -> AnyView {
    match hit.filter(|(s, e)| label.is_char_boundary(*s) && label.is_char_boundary(*e)) {
        Some((s, e)) => view! {
            <span class="prl">{label[..s].to_string()}<b>{label[s..e].to_string()}</b>{label[e..].to_string()}</span>
        }
        .into_any(),
        None => view! { <span class="prl">{label.to_string()}</span> }.into_any(),
    }
}

#[component]
pub fn Jump() -> impl IntoView {
    let c = ctx();
    let query = RwSignal::new(String::new());
    let index = RwSignal::new(0usize);
    let extras = RwSignal::new(Vec::<Extra>::new());
    let input: NodeRef<Input> = NodeRef::new();
    Effect::new(move |_| {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });

    // People and channels you haven't joined come from the server, per online workspace.
    let workspaces: Vec<(Uuid, Uuid, String)> = c.sidebar.with_untracked(|s| {
        s.accounts
            .iter()
            .filter(|a| a.connection == ConnectionState::Online)
            .flat_map(|a| {
                a.workspaces
                    .iter()
                    .map(move |w| (a.id, w.id, w.name.clone()))
            })
            .collect()
    });
    for (account_id, workspace_id, workspace) in workspaces {
        leptos::task::spawn_local(async move {
            let args = json!({"accountId": account_id, "workspaceId": workspace_id});
            let members = ipc::invoke::<Vec<Member>>("workspace_members", args.clone())
                .await
                .unwrap_or_default();
            let channels = ipc::invoke::<Vec<Conversation>>("browse_channels", args)
                .await
                .unwrap_or_default();
            c.remember_names(account_id, &members);
            extras.update(|x| {
                x.push(Extra {
                    account_id,
                    workspace_id,
                    workspace,
                    members,
                    channels,
                })
            });
        });
    }

    let items = Memo::new(move |_| {
        let q = query.get();
        let q = q.trim();
        let here = c.place.get().and_then(|p| p.workspace_id);
        let current = c.selected.get();
        let recent = c.recent.get_value();
        let mut out: Vec<Item> = vec![];
        let mut known: std::collections::HashSet<Uuid> = Default::default();
        let mut dm_peers: std::collections::HashSet<(Uuid, Uuid)> = Default::default();
        c.sidebar.with(|s| {
            for a in &s.accounts {
                let me = a.user_id;
                for w in &a.workspaces {
                    for cv in &w.conversations {
                        known.insert(cv.id);
                        if cv.kind == ConversationKind::Dm
                            && let Some(p) = cv.peer_ids.first()
                        {
                            dm_peers.insert((a.id, *p));
                        }
                        let label = cv.title.trim_start_matches('#').to_string();
                        let Some((rank, hit)) = score(&label, q) else {
                            continue;
                        };
                        let sel = Selection {
                            account_id: a.id,
                            conversation_id: cv.id,
                        };
                        let recency = recent.iter().position(|r| *r == sel);
                        let unread = cv.unread > 0 && !cv.muted;
                        let section = if !q.is_empty() {
                            "Conversations"
                        } else if recency.is_some() {
                            "Recent"
                        } else if unread || cv.mentions > 0 {
                            "Unread"
                        } else {
                            continue;
                        };
                        let mut sub = vec![];
                        if current == Some(sel) {
                            sub.push("current".to_string());
                        } else if cv.mentions > 0 {
                            sub.push(format!(
                                "{} mention{}",
                                cv.mentions,
                                if cv.mentions == 1 { "" } else { "s" }
                            ));
                        } else if unread && is_dm(cv.kind) {
                            sub.push(format!("{} unread", cv.unread));
                        }
                        if here != Some(w.id) {
                            sub.push(w.name.clone());
                        }
                        let glyph = match cv.kind {
                            ConversationKind::Public => Glyph::Icon(Icon::Hash),
                            ConversationKind::Private => Glyph::Icon(Icon::Lock),
                            ConversationKind::Dm => {
                                Glyph::Person(cv.peer_ids.first().copied().unwrap_or(me))
                            }
                            ConversationKind::GroupDm => Glyph::Group(cv.peer_ids.len() + 1),
                        };
                        // Recent first when browsing; otherwise the match decides, recency breaks ties.
                        let bonus = recency.map_or(0, |i| 50 - i as i32);
                        out.push(Item {
                            target: Target::Open(sel),
                            label,
                            glyph,
                            sub: sub.join(" · "),
                            section,
                            rank: if q.is_empty() {
                                bonus * 10 + i32::from(unread)
                            } else {
                                rank + bonus
                            },
                            hit,
                        });
                    }
                }
            }
        });
        if !q.is_empty() {
            extras.with(|xs| {
                for x in xs {
                    let me = c.me(x.account_id).unwrap_or_default();
                    let elsewhere = here != Some(x.workspace_id);
                    for ch in x.channels.iter().filter(|ch| !known.contains(&ch.id)) {
                        let label = ch.name.clone().unwrap_or_default();
                        if let Some((rank, hit)) = score(&label, q) {
                            let mut sub = "Not joined".to_string();
                            if elsewhere {
                                sub.push_str(&format!(" · {}", x.workspace));
                            }
                            out.push(Item {
                                target: Target::Join {
                                    account_id: x.account_id,
                                    conversation_id: ch.id,
                                },
                                label,
                                glyph: Glyph::Icon(Icon::Hash),
                                sub,
                                section: "Conversations",
                                rank: rank - 20,
                                hit,
                            });
                        }
                    }
                    for m in x.members.iter().filter(|m| {
                        m.user_id != me && !dm_peers.contains(&(x.account_id, m.user_id))
                    }) {
                        if let Some((rank, hit)) = score(&m.display_name, q) {
                            out.push(Item {
                                target: Target::Dm {
                                    account_id: x.account_id,
                                    workspace_id: x.workspace_id,
                                    user_id: m.user_id,
                                },
                                label: m.display_name.clone(),
                                glyph: Glyph::Person(m.user_id),
                                sub: if elsewhere {
                                    x.workspace.clone()
                                } else {
                                    String::new()
                                },
                                section: "Conversations",
                                rank: rank - 10,
                                hit,
                            });
                        }
                    }
                }
            });
        }
        let order = |s: &str| match s {
            "Conversations" => 0,
            "Recent" => 1,
            _ => 2,
        };
        out.sort_by(|a, b| {
            order(a.section)
                .cmp(&order(b.section))
                .then(b.rank.cmp(&a.rank))
                .then(a.label.cmp(&b.label))
        });
        out.truncate(14);
        if !q.is_empty() && c.place.get().is_some_and(|p| p.workspace_id.is_some()) {
            out.push(Item {
                target: Target::Search(q.to_string()),
                label: format!("Search messages for “{q}”"),
                glyph: Glyph::Icon(Icon::Search),
                sub: "⌘Return".into(),
                section: "Actions",
                rank: 0,
                hit: None,
            });
        }
        out
    });

    let go = move |t: Target| {
        c.overlay.set(Overlay::None);
        match t {
            Target::Open(sel) => c.open(sel.account_id, sel.conversation_id),
            Target::Dm {
                account_id,
                workspace_id,
                user_id,
            } => c.open_dm(account_id, workspace_id, vec![user_id]),
            Target::Join {
                account_id,
                conversation_id,
            } => leptos::task::spawn_local(async move {
                let args = json!({"accountId": account_id, "conversationId": conversation_id});
                match ipc::invoke::<serde_json::Value>("join_channel", args).await {
                    Ok(_) => c.open_when_cached(account_id, conversation_id),
                    Err(e) => c.notice.set(Some(format!("Could not join: {e}"))),
                }
            }),
            Target::Search(q) => {
                c.query.set(q);
                c.mode.set(Mode::Search);
            }
        }
    };
    let pick = move |i: usize| {
        if let Some(it) = items.with_untracked(|v| v.get(i).cloned()) {
            go(it.target);
        }
    };

    let rows = move || {
        let list = items.get();
        let mut last = "";
        list.into_iter()
            .enumerate()
            .map(|(i, it)| {
                let head = (it.section != last).then(|| {
                    view! { <div class="palsec" role="presentation">{it.section}</div> }
                });
                last = it.section;
                view! {
                    {head}
                    <div class="pr" role="option" id=format!("jump-{i}")
                        aria-selected=move || (index.get() == i).to_string()
                        class:on=move || index.get() == i
                        on:mousemove=move |_| if index.get_untracked() != i { index.set(i) }
                        on:mousedown=move |ev| { ev.prevent_default(); pick(i); }>
                        {glyph_view(&it.glyph, &it.label)}
                        {label_view(&it.label, it.hit)}
                        {(!it.sub.is_empty()).then(|| view! { <span class="sub">{it.sub.clone()}</span> })}
                    </div>
                }
            })
            .collect_view()
    };

    view! {
        <div class="scrim" on:click=move |_| c.overlay.set(Overlay::None)></div>
        <div class="pop pal" role="dialog" aria-modal="true" aria-label="Jump to a conversation">
            <div class="palin">
                {sized(Icon::Search, 16)}
                <input node_ref=input role="combobox" aria-expanded="true" aria-controls="jump-list"
                    aria-activedescendant=move || format!("jump-{}", index.get())
                    aria-label="Jump to a conversation or person" placeholder="Jump to a conversation or person…"
                    prop:value=move || query.get()
                    on:input=move |ev| { query.set(event_target_value(&ev)); index.set(0); }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        let n = items.with_untracked(|v| v.len());
                        match ev.key().as_str() {
                            "ArrowDown" if n > 0 => { ev.prevent_default(); index.update(|i| *i = (*i + 1) % n); }
                            "ArrowUp" if n > 0 => { ev.prevent_default(); index.update(|i| *i = (*i + n - 1) % n); }
                            "Enter" if ev.meta_key() => {
                                ev.prevent_default();
                                let q = query.get_untracked().trim().to_string();
                                if !q.is_empty() { go(Target::Search(q)); }
                            }
                            "Enter" => { ev.prevent_default(); pick(index.get_untracked()); }
                            "Escape" => { ev.prevent_default(); c.overlay.set(Overlay::None); }
                            _ => {}
                        }
                    } />
                <span class="kbd">"esc"</span>
            </div>
            <div class="palbody" id="jump-list" role="listbox" aria-label="Matches">
                {rows}
                {move || (items.with(|v| v.is_empty())).then(|| view! {
                    <p class="palnone">{move || if query.with(|q| q.trim().is_empty()) {
                        "Type a channel or a person's name."
                    } else { "No matches." }}</p>
                })}
            </div>
            <div class="palf"><span>"↑↓ move"</span><span>"Return open"</span><span>"⌘Return search messages"</span>
                <span style="margin-left: auto">"⌘1–9 workspaces"</span></div>
        </div>
    }
}

/// ⌘/: every keyboard shortcut in one place.
#[component]
pub fn Shortcuts() -> impl IntoView {
    let groups: [(&str, &[(&str, &str)]); 3] = [
        (
            "Moving around",
            &[
                ("⌘K", "Jump to a conversation or person"),
                ("⌘F", "Search this workspace"),
                ("⌘⇧F", "Open search"),
                ("⌥↑ ⌥↓", "Previous or next conversation"),
                ("⌥⇧↑ ⌥⇧↓", "Previous or next unread conversation"),
                ("⌘1–9", "Go to a workspace"),
                ("Esc", "Close the thread or panel"),
                ("⌘/", "Show these shortcuts"),
            ],
        ),
        (
            "Writing",
            &[
                ("Return", "Send (inside a ``` block: new line)"),
                ("⌘Return", "Always send"),
                ("⇧Return", "New line"),
                ("↑", "Edit your last message, in an empty composer"),
                ("⌘B ⌘I ⌘E", "Bold, italic, inline code"),
                ("⌘⇧X", "Strikethrough"),
            ],
        ),
        (
            "Messages",
            &[
                ("↑ ↓", "Move between messages, in the list"),
                ("Tab", "Reach a message's actions"),
            ],
        ),
    ];
    view! {
        <Dialog label="Keyboard shortcuts">
            <div class="dbody keys">
                {groups.into_iter().map(|(title, keys)| view! {
                    <div class="sect">{title}</div>
                    <dl>
                        {keys.iter().map(|(k, what)| view! {
                            <div class="krow"><dt><kbd>{*k}</kbd></dt><dd>{*what}</dd></div>
                        }).collect_view()}
                    </dl>
                }).collect_view()}
            </div>
        </Dialog>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_beats_word_start_beats_substring_beats_letters() {
        let p = score("release-planning", "rel").unwrap();
        let w = score("old-release", "rel").unwrap();
        let s = score("unreleased", "rel").unwrap();
        let l = score("really-late", "rel").unwrap();
        assert!(p.0 > w.0 && w.0 > s.0 && s.0 > l.0);
        assert_eq!(p.1, Some((0, 3)));
        assert_eq!(w.1, Some((4, 7)));
        assert_eq!(l.1, None);
        assert!(score("general", "xyz").is_none());
        assert_eq!(score("General", "  ").map(|s| s.0), Some(0));
        // Case doesn't matter; shorter names win among equals.
        assert!(score("Ops", "op").unwrap().0 > score("Operations", "op").unwrap().0);
    }

    #[test]
    fn scoring_500_names_is_fast_enough_to_run_per_keystroke() {
        let names: Vec<String> = (0..500).map(|i| format!("channel-{i}-team")).collect();
        let t = std::time::Instant::now();
        let hits = names.iter().filter(|n| score(n, "c4t").is_some()).count();
        assert!(hits > 0);
        assert!(t.elapsed() < std::time::Duration::from_millis(50));
    }
}
