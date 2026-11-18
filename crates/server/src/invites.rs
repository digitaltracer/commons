//! Single-use invite links to a workspace, each for one email address. Owners and admins
//! create them; the link carries `<invite id>.<secret>`, and only the secret's hash is stored.
use crate::sqlx;
use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use proto::api::{AcceptInvite, CreateInvite, Invite, InviteCreated, InviteList};
use proto::model::{Role, Workspace};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthUser, hash_token, new_token, normalise_email};
use crate::directory::require_role;
use crate::error::{ApiError, ApiResult};
use crate::sync;

const DEFAULT_DAYS: u32 = 7;
const MAX_DAYS: u32 = 30;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/workspaces/{id}/invites", get(list).post(create))
        .route("/workspaces/{id}/invites/{invite_id}", delete(revoke))
        .route("/invites/accept", post(accept))
}

const COLUMNS: &str = "id, workspace_id, email, role, created_by, created_at, expires_at, used_at,
    used_by, revoked_at";

fn row_to_invite(r: &sqlx::postgres::PgRow) -> ApiResult<Invite> {
    let ms = |c: &str| -> ApiResult<Option<i64>> {
        Ok(r.try_get::<Option<time::OffsetDateTime>, _>(c)?
            .map(crate::db::ms))
    };
    let role: String = r.try_get("role")?;
    Ok(Invite {
        id: r.try_get("id")?,
        workspace_id: r.try_get("workspace_id")?,
        email: r.try_get("email")?,
        role: Role::parse(&role)
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("bad role {role}")))?,
        created_by: r.try_get("created_by")?,
        created_at_ms: ms("created_at")?.unwrap_or_default(),
        expires_at_ms: ms("expires_at")?.unwrap_or_default(),
        used_at_ms: ms("used_at")?,
        used_by: r.try_get("used_by")?,
        revoked_at_ms: ms("revoked_at")?,
    })
}

const MANAGERS: &[Role] = &[Role::Owner, Role::Admin];

#[utoipa::path(post, path = "/v1/workspaces/{id}/invites", params(("id" = Uuid, Path)),
    request_body = CreateInvite,
    responses((status = 200, body = InviteCreated),
        (status = 400, description = "invalid email, role or expiry"), (status = 403),
        (status = 409, description = "already a member", body = proto::api::ErrorBody)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<CreateInvite>,
) -> ApiResult<Json<InviteCreated>> {
    let email = normalise_email(&req.email)?;
    if !matches!(req.role, Role::Member | Role::Guest) {
        return Err(ApiError::BadRequest(
            "invites grant the member or guest role".into(),
        ));
    }
    let days = req.expires_in_days.unwrap_or(DEFAULT_DAYS);
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(ApiError::BadRequest(format!(
            "invites expire in 1 to {MAX_DAYS} days"
        )));
    }
    let mut conn = state.pool.acquire().await?;
    require_role(&mut conn, workspace_id, auth.user_id, MANAGERS).await?;
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workspace_members wm JOIN users u ON u.id = wm.user_id
                         WHERE wm.workspace_id = $1 AND u.email = $2 AND wm.left_seq IS NULL)",
    )
    .bind(workspace_id)
    .bind(&email)
    .fetch_one(&mut *conn)
    .await?;
    if member {
        return Err(ApiError::Conflict(serde_json::json!({
            "error": "already_member",
            "message": format!("{email} is already a member of this workspace"),
        })));
    }
    let id = Uuid::now_v7();
    let secret = new_token();
    let sql = format!(
        "INSERT INTO invites (id, token_hash, workspace_id, email, role, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(days => $7)) RETURNING {COLUMNS}"
    );
    let row = sqlx::query(&sql)
        .bind(id)
        .bind(&hash_token(&secret)[..])
        .bind(workspace_id)
        .bind(&email)
        .bind(req.role.as_str())
        .bind(auth.user_id)
        .bind(days as i32)
        .fetch_one(&mut *conn)
        .await?;
    let token = format!("{}.{secret}", id.simple());
    let link = format!(
        "{}/invite#{token}",
        state.config.public_url.trim_end_matches('/')
    );
    Ok(Json(InviteCreated {
        invite: row_to_invite(&row)?,
        token,
        link,
    }))
}

#[utoipa::path(get, path = "/v1/workspaces/{id}/invites", params(("id" = Uuid, Path)),
    responses((status = 200, body = InviteList), (status = 403)))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<InviteList>> {
    let mut conn = state.pool.acquire().await?;
    require_role(&mut conn, workspace_id, auth.user_id, MANAGERS).await?;
    let sql = format!(
        "SELECT {COLUMNS} FROM invites WHERE workspace_id = $1
          ORDER BY created_at DESC LIMIT 200"
    );
    let rows = sqlx::query(&sql)
        .bind(workspace_id)
        .fetch_all(&mut *conn)
        .await?;
    Ok(Json(InviteList {
        invites: rows.iter().map(row_to_invite).collect::<ApiResult<_>>()?,
    }))
}

#[utoipa::path(delete, path = "/v1/workspaces/{id}/invites/{invite_id}",
    params(("id" = Uuid, Path), ("invite_id" = Uuid, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn revoke(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((workspace_id, invite_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut conn = state.pool.acquire().await?;
    require_role(&mut conn, workspace_id, auth.user_id, MANAGERS).await?;
    let found: Option<bool> = sqlx::query_scalar(
        "UPDATE invites SET revoked_at = COALESCE(revoked_at, now())
          WHERE id = $1 AND workspace_id = $2 RETURNING true",
    )
    .bind(invite_id)
    .bind(workspace_id)
    .fetch_optional(&mut *conn)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `<id>.<secret>`, alone or at the end of an invite link (`/invite#…`, or the older
/// `/invite/…`).
fn parse_token(raw: &str) -> Option<(Uuid, String)> {
    let (_, t) = proto::api::split_invite(raw);
    let (id, secret) = t.split_once('.')?;
    Some((Uuid::parse_str(id).ok()?, secret.to_string()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// An invite that passed every check, with its workspace's membership lock held.
pub struct Valid {
    id: Uuid,
    email: String,
    pub workspace: Workspace,
    pub role: Role,
}

/// Checks an invite token and takes the workspace's membership lock. Call it first in the
/// transaction that joins the user, then [`redeem`].
pub async fn prepare(conn: &mut PgConnection, raw: &str) -> ApiResult<Valid> {
    let invalid = || ApiError::Refused("invite_invalid", "this invite link isn't valid".into());
    let (id, secret) = parse_token(raw).ok_or_else(invalid)?;
    let workspace_id: Uuid = sqlx::query_scalar("SELECT workspace_id FROM invites WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(invalid)?;
    sync::lock_workspace(conn, workspace_id).await?;
    // An invite lapses with its creator's authority: it counts as revoked once they are no
    // longer an owner or admin of the workspace.
    type Row = (Vec<u8>, Option<String>, String, bool, bool, bool, String);
    let (hash, email, role, expired, used, revoked, name): Row = sqlx::query_as(
        "SELECT i.token_hash, i.email, i.role, i.expires_at <= now(), i.used_at IS NOT NULL,
                    i.revoked_at IS NOT NULL OR NOT EXISTS (
                        SELECT 1 FROM workspace_members wm
                         WHERE wm.workspace_id = i.workspace_id AND wm.user_id = i.created_by
                           AND wm.left_seq IS NULL AND wm.role IN ('owner', 'admin')),
                    w.name
               FROM invites i JOIN workspaces w ON w.id = i.workspace_id
              WHERE i.id = $1 FOR UPDATE OF i",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if !constant_time_eq(&hash_token(&secret), &hash) {
        return Err(invalid());
    }
    // Invites from before they were bound to an email address are not valid.
    let email = email.ok_or_else(invalid)?;
    if revoked {
        return Err(ApiError::Refused(
            "invite_revoked",
            "this invite was revoked".into(),
        ));
    }
    if used {
        return Err(ApiError::Refused(
            "invite_used",
            "this invite has already been used".into(),
        ));
    }
    if expired {
        return Err(ApiError::Refused(
            "invite_expired",
            "this invite has expired".into(),
        ));
    }
    Ok(Valid {
        id,
        email,
        workspace: Workspace {
            id: workspace_id,
            name,
        },
        role: Role::parse(&role).ok_or_else(invalid)?,
    })
}

/// The refusal for an invite used by someone it wasn't made for.
pub fn email_mismatch() -> ApiError {
    ApiError::Refused(
        "invite_email_mismatch",
        "This invite was made for a different email address.".into(),
    )
}

impl Valid {
    /// Refuses unless `email` (normalised) is the address the invite was made for. The
    /// refusal doesn't say which address that is.
    pub fn check_email(&self, email: &str) -> ApiResult<()> {
        if email != self.email {
            return Err(email_mismatch());
        }
        Ok(())
    }

    /// [`Valid::check_email`] with the email of the account `user_id`.
    pub async fn check_account(&self, conn: &mut PgConnection, user_id: Uuid) -> ApiResult<()> {
        let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&mut *conn)
            .await?;
        self.check_email(&email)
    }
}

/// Uses the invite: marks it used and joins `user_id` to its workspace with its role.
pub async fn redeem(conn: &mut PgConnection, invite: &Valid, user_id: Uuid) -> ApiResult<()> {
    sqlx::query("UPDATE invites SET used_at = now(), used_by = $2 WHERE id = $1")
        .bind(invite.id)
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    let events = crate::directory::join(conn, invite.workspace.id, user_id, invite.role).await?;
    sync::append_join(conn, &invite.workspace, user_id, events).await?;
    Ok(())
}

/// Joins the signed-in user to the invite's workspace.
#[utoipa::path(post, path = "/v1/invites/accept", request_body = AcceptInvite,
    responses((status = 200, body = Workspace),
        (status = 403, description = "the invite is invalid, expired, used or revoked, or \
            was made for a different email address", body = proto::api::ErrorBody),
        (status = 409)))]
pub async fn accept(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AcceptInvite>,
) -> ApiResult<Json<Workspace>> {
    let mut tx = state.pool.begin().await?;
    let invite = prepare(&mut tx, &req.token).await?;
    invite.check_account(&mut tx, auth.user_id).await?;
    redeem(&mut tx, &invite, auth.user_id).await?;
    tx.commit().await?;
    Ok(Json(invite.workspace))
}

/// What an invite link shows in a browser: how to use it in the app.
pub async fn landing() -> Html<&'static str> {
    Html(
        "<!doctype html><meta charset=utf-8><title>Commons invite</title>\
         <h1>You're invited to Commons</h1>\
         <p>Open the Commons app and paste this page's address: in <b>Add a server \
         account</b> to create an account, or <b>Join with an invite link</b> if you \
         already have one on this server.</p>",
    )
}
