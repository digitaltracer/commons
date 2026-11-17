//! How stream events change the cache. Pure decisions over [`Storage`] reads; the caller
//! commits the returned ops together with the cursor advance, in one transaction.
use proto::events::{WorkspaceEvent, WorkspaceEventBody, WorkspaceSnapshot};
use proto::model::Member;
use uuid::Uuid;

use crate::store::{Backfill, Op, Storage};

pub fn workspace_event(
    store: &dyn Storage,
    me: Uuid,
    workspace_id: Uuid,
    ev: &WorkspaceEvent,
) -> anyhow::Result<Vec<Op>> {
    let seq = ev.seq;
    let mut ops = vec![];
    match &ev.body {
        WorkspaceEventBody::ConversationCreated { .. } => {}
        WorkspaceEventBody::MemberAdded {
            conversation,
            user_id,
            ..
        } => {
            let current = store.conversation(conversation.id)?;
            let readable = matches!(current, Some((_, Some(_))));
            if *user_id == me && !readable {
                // Access starts at this seq: backfill everything created before it. The
                // pending backfill is written with the cursor advance, so a crash resumes it.
                ops.push(Op::GrantAccess {
                    conversation: conversation.clone(),
                    access_seq: seq,
                });
                ops.push(Op::AddBackfill(Backfill {
                    conversation_id: conversation.id,
                    interval_seq: seq,
                    until_seq: seq,
                    before_seq: None,
                }));
            } else if readable {
                ops.push(Op::UpsertConversation(conversation.clone()));
            }
        }
        WorkspaceEventBody::MemberRemoved {
            conversation_id,
            user_id,
            has_access,
            ..
        } => {
            if *user_id == me && !has_access {
                ops.push(Op::PurgeConversation(*conversation_id));
            } else if let Some((mut c, Some(_))) = store.conversation(*conversation_id)?
                && c.dm_user_ids.contains(user_id)
                && !has_access
            {
                c.dm_user_ids.retain(|u| u != user_id);
                ops.push(Op::UpsertConversation(c));
            }
        }
        WorkspaceEventBody::MessagePosted { message }
        | WorkspaceEventBody::MessageEdited { message }
        | WorkspaceEventBody::MessageDeleted { message } => {
            ops.push(Op::MergeMessage(message.clone()));
        }
        WorkspaceEventBody::ReactionAdded {
            message_id,
            user_id,
            emoji,
            ..
        } => ops.push(Op::ApplyReaction {
            message_id: *message_id,
            user_id: *user_id,
            emoji: emoji.clone(),
            add: true,
            seq,
        }),
        WorkspaceEventBody::ReactionRemoved {
            message_id,
            user_id,
            emoji,
            ..
        } => ops.push(Op::ApplyReaction {
            message_id: *message_id,
            user_id: *user_id,
            emoji: emoji.clone(),
            add: false,
            seq,
        }),
        WorkspaceEventBody::FileUpdated { file } => {
            for m in store.messages(file.conversation_id)? {
                if let Some(i) = m.attachments.iter().position(|a| a.id == file.id) {
                    let mut m = m;
                    m.attachments[i] = file.clone();
                    // A file update is not a message revision; bump only the change seq.
                    m.updated_seq = m.updated_seq.max(seq);
                    ops.push(Op::MergeMessage(m));
                }
            }
        }
        WorkspaceEventBody::WorkspaceMemberJoined { member } => {
            ops.push(Op::UpsertMember(member.clone()))
        }
        WorkspaceEventBody::WorkspaceMemberLeft { user_id } => ops.push(Op::RemoveMember {
            workspace_id,
            user_id: *user_id,
        }),
        WorkspaceEventBody::MemberRoleChanged { user_id, role } => ops.push(Op::SetMemberRole {
            workspace_id,
            user_id: *user_id,
            role: *role,
        }),
    }
    Ok(ops)
}

/// Replaces a workspace's cached state with a snapshot taken at watermark `snapshot.seq`.
/// Every readable conversation is backfilled up to and including the watermark.
pub fn workspace_snapshot(snapshot: &WorkspaceSnapshot) -> Vec<Op> {
    let ws = snapshot.workspace.id;
    let mut ops = vec![
        Op::ResetWorkspace(ws),
        Op::AddWorkspace {
            workspace: snapshot.workspace.clone(),
            since: snapshot.seq,
        },
    ];
    ops.extend(snapshot.members.iter().cloned().map(Op::UpsertMember));
    for c in &snapshot.conversations {
        ops.push(Op::GrantAccess {
            conversation: c.clone(),
            access_seq: snapshot.seq,
        });
        ops.push(Op::AddBackfill(Backfill {
            conversation_id: c.id,
            interval_seq: snapshot.seq,
            until_seq: snapshot.seq + 1,
            before_seq: None,
        }));
    }
    ops.push(Op::SetWorkspaceSince(ws, snapshot.seq));
    ops
}

pub fn members(members: Vec<Member>) -> Vec<Op> {
    members.into_iter().map(Op::UpsertMember).collect()
}
