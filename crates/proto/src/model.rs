use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Member,
    Guest,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Guest => "guest",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "owner" => Role::Owner,
            "admin" => Role::Admin,
            "member" => Role::Member,
            "guest" => Role::Guest,
            _ => return None,
        })
    }
    /// Roles that imply read access to every private channel in the workspace.
    pub fn reads_private_channels(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Public,
    Private,
    Dm,
    GroupDm,
}

impl ConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConversationKind::Public => "public",
            ConversationKind::Private => "private",
            ConversationKind::Dm => "dm",
            ConversationKind::GroupDm => "group_dm",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "public" => Self::Public,
            "private" => Self::Private,
            "dm" => Self::Dm,
            "group_dm" => Self::GroupDm,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Member {
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub display_name: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Conversation {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub kind: ConversationKind,
    /// Channel name; `None` for DMs and group DMs.
    pub name: Option<String>,
    /// Members of a DM or group DM, so clients can title it. Empty for channels.
    #[serde(default)]
    pub dm_user_ids: Vec<Uuid>,
}

/// A structured entity stored beside the Markdown text. Offsets are UTF-8 byte offsets into `body`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entity {
    Mention {
        user_id: Uuid,
        start: u32,
        end: u32,
    },
    ChannelLink {
        conversation_id: Uuid,
        start: u32,
        end: u32,
    },
    Emoji {
        name: String,
        start: u32,
        end: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Reaction {
    pub emoji: String,
    pub user_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FileInfo {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub name: String,
    pub size: i64,
    pub mime: String,
    pub has_thumbnail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Message {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub conversation_id: Uuid,
    pub author_id: Uuid,
    /// Client-generated UUIDv7 the server deduplicates on.
    pub client_id: Uuid,
    pub thread_root_id: Option<Uuid>,
    pub body: String,
    pub entities: Vec<Entity>,
    pub content_type: String,
    /// Reserved for encrypted DMs; always `None` in v1.
    pub encryption: Option<String>,
    pub attachments: Vec<FileInfo>,
    pub reactions: Vec<Reaction>,
    /// Bumped by every edit and by delete. Edits must name the revision they were based on.
    pub revision: i64,
    pub deleted: bool,
    /// Workspace seq of the event that created the message.
    pub created_seq: i64,
    /// Workspace seq of the last change to this message, including reactions.
    pub updated_seq: i64,
    pub created_at_ms: i64,
    pub edited_at_ms: Option<i64>,
}

impl Message {
    /// Monotonic merge key: a cache keeps whichever state has the larger key.
    pub fn merge_key(&self) -> (i64, i64) {
        (self.revision, self.updated_seq)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotifyLevel {
    All,
    #[default]
    Mentions,
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Preferences {
    /// Default level for channels. DMs always notify unless muted.
    #[serde(default)]
    pub channel_level: NotifyLevel,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub muted: Vec<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotifyReason {
    Mention,
    DirectMessage,
    Keyword,
    AllMessages,
    ThreadReply,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Notification {
    pub message_id: Uuid,
    pub workspace_id: Uuid,
    pub conversation_id: Uuid,
    pub author_id: Uuid,
    pub author_name: String,
    pub reason: NotifyReason,
    /// Plain-text preview, already checked against the recipient's current access.
    pub preview: String,
}
