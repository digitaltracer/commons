//! Durable event streams (decision 9) and the socket frames that deliver them.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{
    Conversation, FileInfo, Member, Message, Notification, Preferences, Role, Workspace,
};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Why a user can read a conversation over an interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum AccessVia {
    Membership,
    Role,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceEventBody {
    /// A public channel was created; visible to every workspace member.
    ConversationCreated {
        conversation: Conversation,
    },
    /// An access interval opened. `has_access` is the user's access after the change.
    MemberAdded {
        conversation: Conversation,
        user_id: Uuid,
        via: AccessVia,
        has_access: bool,
    },
    /// An access interval closed. When `has_access` is false the subject's client purges.
    MemberRemoved {
        conversation_id: Uuid,
        user_id: Uuid,
        via: AccessVia,
        has_access: bool,
    },
    MessagePosted {
        message: Message,
    },
    MessageEdited {
        message: Message,
    },
    /// Tombstone: the message with body and entities cleared.
    MessageDeleted {
        message: Message,
    },
    ReactionAdded {
        conversation_id: Uuid,
        message_id: Uuid,
        user_id: Uuid,
        emoji: String,
    },
    ReactionRemoved {
        conversation_id: Uuid,
        message_id: Uuid,
        user_id: Uuid,
        emoji: String,
    },
    FileUpdated {
        file: FileInfo,
    },
    WorkspaceMemberJoined {
        member: Member,
    },
    WorkspaceMemberLeft {
        user_id: Uuid,
    },
    MemberRoleChanged {
        user_id: Uuid,
        role: Role,
    },
}

impl WorkspaceEventBody {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ConversationCreated { .. } => "conversation_created",
            Self::MemberAdded { .. } => "member_added",
            Self::MemberRemoved { .. } => "member_removed",
            Self::MessagePosted { .. } => "message_posted",
            Self::MessageEdited { .. } => "message_edited",
            Self::MessageDeleted { .. } => "message_deleted",
            Self::ReactionAdded { .. } => "reaction_added",
            Self::ReactionRemoved { .. } => "reaction_removed",
            Self::FileUpdated { .. } => "file_updated",
            Self::WorkspaceMemberJoined { .. } => "workspace_member_joined",
            Self::WorkspaceMemberLeft { .. } => "workspace_member_left",
            Self::MemberRoleChanged { .. } => "member_role_changed",
        }
    }

    pub fn conversation_id(&self) -> Option<Uuid> {
        match self {
            Self::ConversationCreated { conversation } => Some(conversation.id),
            Self::MemberAdded { conversation, .. } => Some(conversation.id),
            Self::MemberRemoved {
                conversation_id, ..
            } => Some(*conversation_id),
            Self::MessagePosted { message }
            | Self::MessageEdited { message }
            | Self::MessageDeleted { message } => Some(message.conversation_id),
            Self::ReactionAdded {
                conversation_id, ..
            }
            | Self::ReactionRemoved {
                conversation_id, ..
            } => Some(*conversation_id),
            Self::FileUpdated { file } => Some(file.conversation_id),
            _ => None,
        }
    }

    /// Fills seq fields that are only known once the event row commits.
    pub fn stamp(&mut self, seq: i64) {
        match self {
            Self::MessagePosted { message } => {
                message.created_seq = seq;
                message.updated_seq = seq;
            }
            Self::MessageEdited { message } | Self::MessageDeleted { message } => {
                message.updated_seq = seq;
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WorkspaceEvent {
    pub seq: i64,
    pub body: WorkspaceEventBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserEventBody {
    /// The user joined a workspace. Resume its stream from `start_seq - 1`.
    WorkspaceJoined {
        workspace: Workspace,
        start_seq: i64,
    },
    /// The user was removed from a workspace. The client purges it.
    WorkspaceRemoved {
        workspace_id: Uuid,
    },
    ReadCursor {
        workspace_id: Uuid,
        conversation_id: Uuid,
        seq: i64,
    },
    Notify {
        notification: Notification,
    },
    PreferencesChanged {
        preferences: Preferences,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UserEvent {
    pub seq: i64,
    pub body: UserEventBody,
}

/// What a user can read in a workspace at watermark `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WorkspaceSnapshot {
    pub seq: i64,
    pub workspace: Workspace,
    pub members: Vec<Member>,
    /// Conversations the user can read at `seq`.
    pub conversations: Vec<Conversation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UserSnapshot {
    pub seq: i64,
    /// Each workspace with the seq its stream should resume after.
    pub workspaces: Vec<(Workspace, i64)>,
    pub read_cursors: Vec<ReadCursorState>,
    pub preferences: Preferences,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReadCursorState {
    pub workspace_id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
}

/// `GET /v1/sync/workspaces/{id}?since=`. `next_since` always advances past filtered events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WorkspaceReplay {
    pub events: Vec<WorkspaceEvent>,
    pub next_since: i64,
    pub head: i64,
    /// Present when `since` was before the prune point; replay continues after `snapshot.seq`.
    pub snapshot: Option<WorkspaceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UserReplay {
    pub events: Vec<UserEvent>,
    pub next_since: i64,
    pub head: i64,
    pub snapshot: Option<UserSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamCursor {
    pub workspace_id: Uuid,
    pub since: i64,
}

/// Frames the client sends on the socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello {
        user_since: i64,
        workspaces: Vec<StreamCursor>,
    },
    Typing {
        conversation_id: Uuid,
    },
}

/// Frames the server sends on the socket. The socket only delivers; HTTPS replay is the truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Workspace {
        workspace_id: Uuid,
        events: Vec<WorkspaceEvent>,
        next_since: i64,
    },
    User {
        events: Vec<UserEvent>,
        next_since: i64,
    },
    /// The cursor is behind the prune point; replay over HTTPS to get a snapshot.
    Resync {
        workspace_id: Option<Uuid>,
    },
    Presence {
        user_id: Uuid,
        online: bool,
    },
    /// Who is online in a workspace, sent once this socket receives its presence frames, so
    /// no change can fall between this list and the frames that follow it.
    Online {
        workspace_id: Uuid,
        user_ids: Vec<Uuid>,
    },
    Typing {
        workspace_id: Uuid,
        conversation_id: Uuid,
        user_id: Uuid,
    },
}
