//! View models the core renders for the UI. The UI never sees tokens, SQL or sockets.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::SearchHitKind;
use crate::model::{ConversationKind, Entity, Reaction, Role};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendState {
    Sent,
    Sending,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentView {
    pub file_id: Uuid,
    pub name: String,
    pub size: i64,
    pub mime: String,
    /// `commons-file://` URL the webview loads; the core attaches the session.
    pub url: String,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageView {
    /// Server id, or the client id while the send is pending.
    pub id: Uuid,
    pub author_id: Uuid,
    pub author_name: String,
    pub body: String,
    pub entities: Vec<Entity>,
    pub created_at_ms: i64,
    pub edited: bool,
    pub deleted: bool,
    pub revision: i64,
    pub state: SendState,
    pub reactions: Vec<Reaction>,
    pub reply_count: u32,
    pub attachments: Vec<AttachmentView>,
    pub mine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationView {
    pub id: Uuid,
    pub kind: ConversationKind,
    pub title: String,
    pub unread: u32,
    pub mentions: u32,
    /// Muted in the user's notification preferences.
    #[serde(default)]
    pub muted: bool,
    /// Sends waiting in the outbox.
    #[serde(default)]
    pub pending: u32,
    /// The other people in a DM or group DM; empty for channels.
    #[serde(default)]
    pub peer_ids: Vec<Uuid>,
    /// Unsent text waits in this conversation's composer or one of its threads'.
    #[serde(default)]
    pub has_draft: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: Uuid,
    pub name: String,
    pub role: Option<Role>,
    pub conversations: Vec<ConversationView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Offline,
    Connecting,
    Online,
    SignedOut,
}

/// Why an account is signed out, as far as this device can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnd {
    /// The server refused the saved session (401).
    Revoked,
    /// This device has no session token for the account.
    NoToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    pub id: Uuid,
    pub server_url: String,
    pub email: String,
    pub display_name: String,
    pub user_id: Uuid,
    pub connection: ConnectionState,
    /// When the next reconnect attempt is due, while offline.
    #[serde(default)]
    pub reconnect_at_ms: Option<i64>,
    /// Set while signed out.
    #[serde(default)]
    pub session_end: Option<SessionEnd>,
    pub workspaces: Vec<WorkspaceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarView {
    pub accounts: Vec<AccountView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftReason {
    /// The message changed after the edit was started.
    StaleEdit,
    /// The message was deleted before the edit landed.
    Deleted,
    /// A send was rejected or gave up after retrying.
    SendFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftView {
    pub id: Uuid,
    pub account_id: Uuid,
    pub conversation_id: Uuid,
    pub message_id: Option<Uuid>,
    pub body: String,
    pub reason: DraftReason,
}

/// A run of snippet text; `hit` marks the matched terms. Rendered as text only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnippetPart {
    pub text: String,
    pub hit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHitView {
    pub kind: SearchHitKind,
    pub conversation_id: Uuid,
    pub conversation_title: String,
    /// False for a public channel the user can search but has not joined.
    pub joined: bool,
    pub message_id: Option<Uuid>,
    pub file_name: Option<String>,
    pub snippet: Vec<SnippetPart>,
    pub created_at_ms: i64,
    #[serde(default)]
    pub author_id: Option<Uuid>,
    #[serde(default)]
    pub author_name: Option<String>,
    /// The hit is a reply inside a thread.
    #[serde(default)]
    pub in_thread: bool,
}

/// Low-volume app events pushed over the app channel. Message rows travel only in windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    Sidebar {
        sidebar: SidebarView,
    },
    Drafts {
        drafts: Vec<DraftView>,
    },
    Typing {
        account_id: Uuid,
        conversation_id: Uuid,
        names: Vec<String>,
    },
    Presence {
        account_id: Uuid,
        online: Vec<Uuid>,
    },
    Error {
        message: String,
    },
    /// The shell downloaded this version; restarting installs it.
    UpdateReady {
        version: String,
    },
}
