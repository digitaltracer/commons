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
    /// Set when the server offers sign-in through an OIDC provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcMeta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OidcMeta {
    /// The provider's name for the button, as in "Continue with Google".
    pub name: String,
}

/// Starts an OIDC sign-in. The app opens `authorize_url` in the system browser; the browser
/// comes back to `return_to` with `code`, or with `error` and `message`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OidcStart {
    /// A loopback address the app listens on, `http://127.0.0.1:<port>/<path>` (RFC 8252).
    pub return_to: String,
    /// The app's PKCE challenge for the handoff: base64url(SHA-256(verifier)), unpadded.
    pub code_challenge: String,
    /// An invite token or link, for a new account in invite mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
    /// Link the provider identity to the signed-in caller instead of signing in.
    #[serde(default)]
    pub link: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OidcStarted {
    pub authorize_url: String,
}

/// Exchanges the handoff code from the return redirect, with the verifier for its challenge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OidcExchange {
    pub code: String,
    pub code_verifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OidcExchanged {
    /// A sign-in: a new session for the account.
    Session { session: SessionResponse },
    /// A link: the caller's identities, including the new one.
    Linked { identities: IdentityList },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OidcIdentity {
    pub id: Uuid,
    pub issuer: String,
    pub email: Option<String>,
    pub created_at_ms: i64,
}

/// The caller's ways to sign in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct IdentityList {
    pub has_password: bool,
    pub identities: Vec<OidcIdentity>,
}

/// A workspace member as owners and admins see them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AdminMember {
    pub user_id: Uuid,
    pub display_name: String,
    pub role: Role,
    pub joined_at_ms: i64,
    /// Connected to this server node now.
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AdminMemberList {
    pub members: Vec<AdminMember>,
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

/// Creates a single-use invite to a workspace for one email address. Owners and admins only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CreateInvite {
    /// Only an account with this email can use the invite.
    pub email: String,
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
    /// The email it is for, normalised. None on invites made before invites had one; those
    /// are not valid.
    #[serde(default)]
    pub email: Option<String>,
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

/// Whether `s` has the shape of an invite token: `<invite id as 32 hex>.<43-char secret>`.
pub fn is_invite_token(s: &str) -> bool {
    let Some((id, secret)) = s.split_once('.') else {
        return false;
    };
    id.len() == 32
        && id.bytes().all(|b| b.is_ascii_hexdigit())
        && secret.len() == 43
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Asks what an invite is for, before anyone signs in. The token is in the body so it stays
/// out of request lines and access logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct InvitePreviewRequest {
    pub token: String,
}

/// What a valid invite offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct InvitePreview {
    pub workspace_name: String,
    pub inviter_name: String,
    pub role: Role,
    pub expires_at_ms: i64,
    /// The invited address with most of its local part hidden, such as `ad•••@gmail.com`.
    pub email_masked: String,
    /// The server's sign-in provider, as in [`Meta`], so the app needn't ask separately.
    #[serde(default)]
    pub oidc: Option<OidcMeta>,
}

#[cfg(test)]
mod tests {
    use super::{is_invite_token, split_invite};

    #[test]
    fn invite_token_shape() {
        let secret = "A".repeat(43);
        let id = "0192f0c1a2b34c5d8e9f0a1b2c3d4e5f";
        assert!(is_invite_token(&format!("{id}.{secret}")));
        assert!(is_invite_token(&format!("{id}.{}-_9", "x".repeat(40))));
        for bad in [
            String::new(),
            format!("{id}{secret}"),
            format!("{id}.{}", "A".repeat(42)),
            format!("{id}.{secret}A"),
            format!("{id}.{}=", "A".repeat(42)),
            format!("{}.{secret}", &id[1..]),
            format!("{}g.{secret}", &id[1..]),
            format!("{id}.{secret}.x"),
        ] {
            assert!(!is_invite_token(&bad), "{bad}");
        }
    }

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
