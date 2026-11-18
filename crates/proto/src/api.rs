//! Request and response bodies for the `/v1` REST resources.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{
    Conversation, ConversationKind, Entity, FileInfo, Member, Message, Role, User, Workspace,
};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ErrorBody {
    pub error: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Meta {
    pub protocol_version: u32,
    pub server_version: String,
    pub min_client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
    pub display_name: String,
    /// An invite token or link. Required when the server's registration mode is `invite`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SessionResponse {
    pub token: String,
    pub user: User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ResetRequest {
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ResetConfirm {
    pub token: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CreateWorkspace {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WorkspaceList {
    pub workspaces: Vec<Workspace>,
}

/// Creates a single-use invite to a workspace. Owners and admins only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CreateInvite {
    /// `member` or `guest`.
    pub role: Role,
    /// Days until it expires: 1 to 30, 7 by default.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Invite {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub role: Role,
    pub created_by: Uuid,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub used_at_ms: Option<i64>,
    pub used_by: Option<Uuid>,
    pub revoked_at_ms: Option<i64>,
}

/// A new invite. The token is shown this once; the server keeps only its hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct InviteCreated {
    pub invite: Invite,
    pub token: String,
    /// `<public_url>/invite#<token>`; the token is in the fragment, which browsers never send.
    pub link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct InviteList {
    pub invites: Vec<Invite>,
}

/// Splits a pasted invite into the server URL (when it is a whole link) and the token.
/// Takes `<server>/invite#<token>`, the older `<server>/invite/<token>`, or a bare token.
pub fn split_invite(pasted: &str) -> (Option<String>, String) {
    let p = pasted.trim().trim_end_matches('/');
    let split = p
        .rsplit_once("/invite#")
        .or_else(|| p.rsplit_once("/invite/"));
    match split {
        Some((server, token)) => (
            Some(server.to_string()),
            token.trim_start_matches('#').to_string(),
        ),
        None => (None, p.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::split_invite;

    #[test]
    fn invite_links_old_links_and_bare_tokens() {
        let server = Some("https://chat.example.org".to_string());
        assert_eq!(
            split_invite("https://chat.example.org/invite#abc.DEF-_1"),
            (server.clone(), "abc.DEF-_1".to_string())
        );
        assert_eq!(
            split_invite(" https://chat.example.org/invite/abc.DEF-_1/ "),
            (server, "abc.DEF-_1".to_string())
        );
        assert_eq!(split_invite("abc.DEF-_1"), (None, "abc.DEF-_1".to_string()));
    }
}

/// Accepts an invite as the signed-in user: an invite token, or the whole link.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AcceptInvite {
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AddWorkspaceMember {
    pub email: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SetRole {
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MemberList {
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CreateChannel {
    pub name: String,
    pub kind: ConversationKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OpenDm {
    pub user_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ConversationList {
    pub conversations: Vec<Conversation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AddConversationMember {
    pub user_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SendMessage {
    /// Client-generated UUIDv7; resending the same id does nothing.
    pub client_id: Uuid,
    pub body: String,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub thread_root_id: Option<Uuid>,
    #[serde(default)]
    pub attachments: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct EditMessage {
    pub body: String,
    #[serde(default)]
    pub entities: Vec<Entity>,
    /// The revision this edit was based on. A stale base is rejected with 409.
    pub base_revision: i64,
}

/// Body of a 409 (stale edit) or 410 (deleted) response to an edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct EditConflict {
    pub error: String,
    pub current: Message,
}

/// A page of conversation history, served for backfill.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MessagePage {
    pub messages: Vec<Message>,
    /// Echo of the access interval the page was requested for.
    pub interval_seq: i64,
    /// Pass as `before_seq` for the next page; `None` when history is exhausted.
    pub next_before_seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MarkRead {
    pub seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UploadResponse {
    pub file: FileInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SearchHitKind {
    Message,
    FileName,
    FileText,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SearchHit {
    pub kind: SearchHitKind,
    pub conversation_id: Uuid,
    /// Channel name, so hits in public channels the user has not joined can be titled.
    #[serde(default)]
    pub conversation_name: Option<String>,
    pub message_id: Option<Uuid>,
    pub file_id: Option<Uuid>,
    #[serde(default)]
    pub file_name: Option<String>,
    /// Snippet text; matched terms are wrapped in U+0002 … U+0003. Render as text only.
    pub snippet: String,
    pub created_at_ms: i64,
    /// Author of the message, or of the message that carries the file.
    #[serde(default)]
    pub author_id: Option<Uuid>,
    /// Set when the hit is a thread reply.
    #[serde(default)]
    pub thread_root_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
}

pub const SNIPPET_START: char = '\u{2}';
pub const SNIPPET_STOP: char = '\u{3}';
