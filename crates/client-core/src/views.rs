//! Turns cached state into the view models the UI renders.
use std::collections::{BTreeMap, HashMap};

use proto::api::{SNIPPET_START, SNIPPET_STOP, SearchHit};
use proto::model::{Conversation, ConversationKind, FileInfo, Message, Preferences};
use proto::view::{
    AttachmentView, ConversationView, MessageView, SearchHitView, SendState, SnippetPart,
    WorkspaceView,
};
use uuid::Uuid;

use crate::store::{OutboxItem, OutboxKind, OutboxState, ReplySummary, Storage, WindowItem};

/// URL the webview loads; served by the shell's `commons-file` protocol through the core.
pub fn file_url(account_id: Uuid, file_id: Uuid, thumbnail: bool) -> String {
    let suffix = if thumbnail { "/thumbnail" } else { "" };
    format!("commons-file://localhost/{account_id}/{file_id}{suffix}")
}

pub fn attachment(account_id: Uuid, f: &FileInfo) -> AttachmentView {
    AttachmentView {
        file_id: f.id,
        name: f.name.clone(),
        size: f.size,
        mime: f.mime.clone(),
        url: file_url(account_id, f.id, false),
        thumbnail_url: f.has_thumbnail.then(|| file_url(account_id, f.id, true)),
    }
}

pub struct Names(pub HashMap<Uuid, String>);

impl Names {
    pub fn get(&self, id: Uuid) -> String {
        self.0.get(&id).cloned().unwrap_or_else(|| "Unknown".into())
    }
}

pub fn message_view(
    account_id: Uuid,
    me: Uuid,
    m: &Message,
    names: &Names,
    replies: Option<&ReplySummary>,
) -> MessageView {
    MessageView {
        id: m.client_id,
        author_id: m.author_id,
        author_name: names.get(m.author_id),
        body: m.body.clone(),
        entities: m.entities.clone(),
        created_at_ms: m.created_at_ms,
        edited: m.edited_at_ms.is_some(),
        deleted: m.deleted,
        revision: m.revision,
        state: SendState::Sent,
        reactions: m.reactions.clone(),
        reply_count: replies.map_or(0, |r| r.count),
        last_reply_at_ms: replies.map(|r| r.last_at_ms),
        reply_user_ids: replies.map(|r| r.authors.clone()).unwrap_or_default(),
        attachments: m
            .attachments
            .iter()
            .map(|f| attachment(account_id, f))
            .collect(),
        mine: m.author_id == me,
    }
}

pub fn pending_view(me: Uuid, o: &OutboxItem, names: &Names) -> Option<MessageView> {
    let OutboxKind::Send { body, entities, .. } = &o.kind else {
        return None;
    };
    Some(MessageView {
        id: o.id,
        author_id: me,
        author_name: names.get(me),
        body: body.clone(),
        entities: entities.clone(),
        created_at_ms: o.created_at_ms,
        edited: false,
        deleted: false,
        revision: 0,
        state: match o.state {
            OutboxState::Pending => SendState::Sending,
            OutboxState::Failed => SendState::Failed {
                reason: o.last_error.clone().unwrap_or_else(|| "not sent".into()),
            },
        },
        reactions: vec![],
        reply_count: 0,
        last_reply_at_ms: None,
        reply_user_ids: vec![],
        attachments: vec![],
        mine: true,
    })
}

pub fn window_views(
    account_id: Uuid,
    me: Uuid,
    store: &dyn Storage,
    items: Vec<WindowItem>,
    names: &Names,
) -> anyhow::Result<Vec<MessageView>> {
    let roots: Vec<Uuid> = items
        .iter()
        .filter_map(|i| match i {
            WindowItem::Message(m) if m.thread_root_id.is_none() => Some(m.id),
            _ => None,
        })
        .collect();
    let replies = store.reply_summaries(&roots)?;
    Ok(items
        .iter()
        .filter_map(|i| match i {
            WindowItem::Message(m) => {
                Some(message_view(account_id, me, m, names, replies.get(&m.id)))
            }
            WindowItem::Pending(o) => pending_view(me, o, names),
        })
        .collect())
}

/// "#name" for channels; the other members' names for DMs.
pub fn conversation_title(c: &Conversation, names: &HashMap<Uuid, String>, me: Uuid) -> String {
    match c.kind {
        ConversationKind::Public | ConversationKind::Private => {
            format!("#{}", c.name.clone().unwrap_or_default())
        }
        ConversationKind::Dm | ConversationKind::GroupDm => {
            let others: Vec<String> = c
                .dm_user_ids
                .iter()
                .filter(|u| **u != me)
                .map(|u| names.get(u).cloned().unwrap_or_else(|| "Unknown".into()))
                .collect();
            if others.is_empty() {
                "Just you".into()
            } else {
                others.join(", ")
            }
        }
    }
}

/// Splits a server snippet at its U+0002 / U+0003 markers.
pub fn snippet_parts(snippet: &str) -> Vec<SnippetPart> {
    let mut parts = vec![];
    let mut hit = false;
    let mut text = String::new();
    for ch in snippet.chars() {
        let toggle = match ch {
            SNIPPET_START => Some(true),
            SNIPPET_STOP => Some(false),
            _ => None,
        };
        match toggle {
            Some(next) => {
                if !text.is_empty() {
                    parts.push(SnippetPart {
                        text: std::mem::take(&mut text),
                        hit,
                    });
                }
                hit = next;
            }
            None => text.push(ch),
        }
    }
    if !text.is_empty() {
        parts.push(SnippetPart { text, hit });
    }
    parts
}

pub fn search_hit_view(
    store: &dyn Storage,
    me: Uuid,
    names: &HashMap<Uuid, String>,
    h: &SearchHit,
) -> anyhow::Result<SearchHitView> {
    let cached = store.conversation(h.conversation_id)?;
    let joined = matches!(cached, Some((_, Some(_))));
    let conversation_title = match &cached {
        Some((c, Some(_))) => conversation_title(c, names, me),
        _ => format!("#{}", h.conversation_name.clone().unwrap_or_default()),
    };
    Ok(SearchHitView {
        kind: h.kind,
        conversation_id: h.conversation_id,
        conversation_title,
        joined,
        message_id: h.message_id,
        file_name: h.file_name.clone(),
        snippet: snippet_parts(&h.snippet),
        created_at_ms: h.created_at_ms,
        author_id: h.author_id,
        author_name: h
            .author_id
            .map(|a| names.get(&a).cloned().unwrap_or_else(|| "Unknown".into())),
        in_thread: h.thread_root_id.is_some(),
    })
}

/// The user's notification preferences as last synced; defaults before the first sync.
pub fn preferences(store: &dyn Storage) -> anyhow::Result<Preferences> {
    Ok(store
        .meta("preferences")?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// Meta key holding unsent composer text for this account.
pub const COMPOSER_DRAFTS: &str = "composer_drafts";

/// "<conversation>" for the main composer, "<conversation>/<thread root>" for a thread's.
pub fn draft_key(conversation_id: Uuid, thread_root_id: Option<Uuid>) -> String {
    match thread_root_id {
        Some(t) => format!("{conversation_id}/{t}"),
        None => conversation_id.to_string(),
    }
}

/// Unsent composer text by [`draft_key`].
pub fn composer_drafts(store: &dyn Storage) -> anyhow::Result<BTreeMap<String, String>> {
    Ok(store
        .meta(COMPOSER_DRAFTS)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// Conversations with unsent text in their own composer or one of their threads'.
pub fn drafted(drafts: &BTreeMap<String, String>) -> std::collections::HashSet<Uuid> {
    drafts
        .keys()
        .filter_map(|k| k.split('/').next()?.parse().ok())
        .collect()
}

pub fn workspace_views(store: &dyn Storage, me: Uuid) -> anyhow::Result<Vec<WorkspaceView>> {
    let prefs = preferences(store)?;
    let drafted = drafted(&composer_drafts(store)?);
    let mut pending: HashMap<Uuid, u32> = HashMap::new();
    for o in store.outbox()? {
        if matches!(o.kind, OutboxKind::Send { .. }) && o.state == OutboxState::Pending {
            *pending.entry(o.conversation_id).or_default() += 1;
        }
    }
    let mut out = vec![];
    for (w, _) in store.workspaces()? {
        let members = store.members(w.id)?;
        let names: HashMap<Uuid, String> = members
            .iter()
            .map(|m| (m.user_id, m.display_name.clone()))
            .collect();
        let role = members.iter().find(|m| m.user_id == me).map(|m| m.role);
        let mut conversations = vec![];
        for (c, access) in store.conversations(w.id)? {
            if access.is_none() {
                continue;
            }
            let title = conversation_title(&c, &names, me);
            let (unread, mentions) = store.unread(c.id, me)?;
            conversations.push(ConversationView {
                id: c.id,
                kind: c.kind,
                title,
                unread,
                mentions,
                muted: prefs.muted.contains(&c.id),
                pending: pending.get(&c.id).copied().unwrap_or(0),
                peer_ids: c.dm_user_ids.iter().copied().filter(|u| *u != me).collect(),
                has_draft: drafted.contains(&c.id),
            });
        }
        out.push(WorkspaceView {
            id: w.id,
            name: w.name,
            role,
            conversations,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_split_at_markers() {
        let p = snippet_parts("meet at the \u{2}café\u{3} on \u{2}Tuesday\u{3}");
        let got: Vec<(&str, bool)> = p.iter().map(|p| (p.text.as_str(), p.hit)).collect();
        assert_eq!(
            got,
            vec![
                ("meet at the ", false),
                ("café", true),
                (" on ", false),
                ("Tuesday", true)
            ]
        );
        assert!(snippet_parts("").is_empty());
        assert_eq!(snippet_parts("<b>x</b>")[0].text, "<b>x</b>");
    }
}
