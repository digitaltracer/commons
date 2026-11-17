//! Turns cached state into the view models the UI renders.
use std::collections::HashMap;

use proto::model::{ConversationKind, FileInfo, Message};
use proto::view::{AttachmentView, ConversationView, MessageView, SendState, WorkspaceView};
use uuid::Uuid;

use crate::store::{OutboxItem, OutboxKind, OutboxState, Storage, WindowItem};

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
    replies: u32,
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
        reply_count: replies,
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
    let replies = store.reply_counts(&roots)?;
    Ok(items
        .iter()
        .filter_map(|i| match i {
            WindowItem::Message(m) => Some(message_view(
                account_id,
                me,
                m,
                names,
                replies.get(&m.id).copied().unwrap_or(0),
            )),
            WindowItem::Pending(o) => pending_view(me, o, names),
        })
        .collect())
}

pub fn workspace_views(store: &dyn Storage, me: Uuid) -> anyhow::Result<Vec<WorkspaceView>> {
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
            let title = match c.kind {
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
            };
            let (unread, mentions) = store.unread(c.id, me)?;
            conversations.push(ConversationView {
                id: c.id,
                kind: c.kind,
                title,
                unread,
                mentions,
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
