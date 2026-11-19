//! Single-use invite links to a workspace, each for one email address. Owners and admins
//! create them; the link carries `<invite id>.<secret>`, and only the secret's hash is stored.
use crate::sqlx;
use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use proto::api::{
    AcceptInvite, CreateInvite, Invite, InviteCreated, InviteList, InvitePreview,
    InvitePreviewRequest,
};
use proto::model::{Role, Workspace};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthUser, ClientIp, hash_token, new_token, normalise_email};
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
        .route("/invites/preview", post(preview))
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

fn invalid() -> ApiError {
    ApiError::Refused("invite_invalid", "this invite link isn't valid".into())
}

/// An invite's row, read with or without its row lock.
struct Loaded {
    workspace: Workspace,
    email: String,
    role: Role,
    inviter: String,
    expires_at: time::OffsetDateTime,
}

/// Reads the invite `id` and refuses it unless `secret` matches and it is open: not revoked,
/// used or expired. `lock` takes its row lock, for the transaction that redeems it.
async fn load(conn: &mut PgConnection, id: Uuid, secret: &str, lock: bool) -> ApiResult<Loaded> {
    // An invite lapses with its creator's authority: it counts as revoked once they are no
    // longer an owner or admin of the workspace.
    let sql = format!(
        "SELECT i.token_hash, i.email, i.role, i.expires_at, i.expires_at <= now(),
                i.used_at IS NOT NULL,
                i.revoked_at IS NOT NULL OR NOT EXISTS (
                    SELECT 1 FROM workspace_members wm
                     WHERE wm.workspace_id = i.workspace_id AND wm.user_id = i.created_by
                       AND wm.left_seq IS NULL AND wm.role IN ('owner', 'admin')),
                w.id, w.name, u.display_name
           FROM invites i JOIN workspaces w ON w.id = i.workspace_id
           JOIN users u ON u.id = i.created_by
          WHERE i.id = $1{}",
        if lock { " FOR UPDATE OF i" } else { "" }
    );
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(invalid)?;
    let hash: Vec<u8> = row.try_get(0)?;
    if !constant_time_eq(&hash_token(secret), &hash) {
        return Err(invalid());
    }
    // Invites from before they were bound to an email address are not valid.
    let email: String = row.try_get::<Option<String>, _>(1)?.ok_or_else(invalid)?;
    let role: String = row.try_get(2)?;
    let (expired, used, revoked): (bool, bool, bool) =
        (row.try_get(4)?, row.try_get(5)?, row.try_get(6)?);
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
    Ok(Loaded {
        workspace: Workspace {
            id: row.try_get(7)?,
            name: row.try_get(8)?,
        },
        email,
        role: Role::parse(&role).ok_or_else(invalid)?,
        inviter: row.try_get(9)?,
        expires_at: row.try_get(3)?,
    })
}

/// Checks an invite token and takes the workspace's membership lock. Call it first in the
/// transaction that joins the user, then [`redeem`].
pub async fn prepare(conn: &mut PgConnection, raw: &str) -> ApiResult<Valid> {
    let (id, secret) = parse_token(raw).ok_or_else(invalid)?;
    let workspace_id: Uuid = sqlx::query_scalar("SELECT workspace_id FROM invites WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(invalid)?;
    sync::lock_workspace(conn, workspace_id).await?;
    let l = load(conn, id, &secret, true).await?;
    Ok(Valid {
        id,
        email: l.email,
        workspace: l.workspace,
        role: l.role,
    })
}

/// `ad•••@gmail.com`: the first two characters of the local part (one, for a short one),
/// so a forwarded link doesn't give the whole address away.
pub fn mask_email(email: &str) -> String {
    let (local, domain) = email.rsplit_once('@').unwrap_or((email, ""));
    let keep = match local.chars().count() {
        0 | 1 => 0,
        2 | 3 => 1,
        _ => 2,
    };
    let shown: String = local.chars().take(keep).collect();
    format!("{shown}\u{2022}\u{2022}\u{2022}@{domain}")
}

/// What an invite is for, before anyone signs in: read-only, and without the locks an
/// accept takes. Every kind of refusal looks the same.
#[utoipa::path(post, path = "/v1/invites/preview", request_body = InvitePreviewRequest,
    responses((status = 200, body = InvitePreview), (status = 429),
        (status = 403, description = "the invite is invalid, expired, used or revoked",
            body = proto::api::ErrorBody)))]
pub async fn preview(
    State(state): State<AppState>,
    ip: ClientIp,
    Json(req): Json<InvitePreviewRequest>,
) -> ApiResult<Json<InvitePreview>> {
    ip.check(&state, "invite-preview")?;
    let refused = || {
        ApiError::Refused(
            "invite_invalid",
            "This invite link isn't valid. It may have expired, been used or been revoked. \
             Ask for a new one."
                .into(),
        )
    };
    let (id, secret) = parse_token(&req.token)
        .filter(|_| req.token.len() <= 256)
        .ok_or_else(refused)?;
    let mut conn = state.pool.acquire().await?;
    let l = match load(&mut conn, id, &secret, false).await {
        Ok(l) => l,
        Err(ApiError::Refused(..)) => return Err(refused()),
        Err(e) => return Err(e),
    };
    Ok(Json(InvitePreview {
        workspace_name: l.workspace.name,
        inviter_name: l.inviter,
        role: l.role,
        expires_at_ms: crate::db::ms(l.expires_at),
        email_masked: mask_email(&l.email),
    }))
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

/// What an invite link shows in a browser: a button that opens the app on the invite, a
/// download link, and how to paste it by hand. The token stays in the fragment; the page's
/// script hands it to the app as `commons://invite?server=<origin>#<token>`.
pub async fn landing(State(state): State<AppState>) -> impl IntoResponse {
    let page = LANDING_PAGE
        .replace("{script}", LANDING_SCRIPT)
        .replace("{download}", &escape_html(&state.config.app_download_url));
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, landing_csp()),
        ],
        Html(page),
    )
}

/// Only the page's own script runs, matched by its hash.
pub fn landing_csp() -> &'static str {
    static CSP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CSP.get_or_init(|| {
        use base64::Engine;
        use sha2::Digest;
        let hash = base64::engine::general_purpose::STANDARD
            .encode(sha2::Sha256::digest(LANDING_SCRIPT.as_bytes()));
        format!(
            "default-src 'none'; script-src 'sha256-{hash}'; style-src 'unsafe-inline'; \
             base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        )
    })
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const LANDING_PAGE: &str = r#"<!doctype html><html lang=en><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<meta name=color-scheme content="light dark">
<title>You're invited to Commons</title>
<style>
:root{--bg:#f6f5f2;--card:#fff;--fg:#1d1c1a;--muted:#6b6862;--line:#e4e1da;--accent:#2f6f4f;--accent-fg:#fff}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--card:#201f1d;--fg:#eceae5;--muted:#a19d95;--line:#34322e;--accent:#5fb487;--accent-fg:#0f1a14}}
*{box-sizing:border-box}
[hidden]{display:none!important}
body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,-apple-system,sans-serif;padding:16px}
main{width:100%;max-width:26rem;background:var(--card);border:1px solid var(--line);border-radius:14px;padding:2rem}
h1{font-size:1.4rem;margin:0 0 .5rem}
p{margin:.5rem 0;color:var(--muted)}
.actions{display:grid;gap:.6rem;margin:1.5rem 0 1rem}
.btn{display:block;width:100%;text-align:center;padding:.7rem 1rem;border-radius:9px;font:inherit;font-weight:600;text-decoration:none;cursor:pointer;border:1px solid var(--line);background:transparent;color:var(--fg)}
.btn.primary{background:var(--accent);border-color:var(--accent);color:var(--accent-fg)}
.btn:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
.small{font-size:.875rem}
#bad{color:var(--fg)}
</style>
<main>
<h1>You're invited to Commons</h1>
<p>Open the invite in the Commons app to create your account or join with the one you have.</p>
<div class=actions>
<button id=open class="btn primary" type=button hidden>Open in Commons</button>
<a class=btn href="{download}" rel=noopener>Download for Mac</a>
</div>
<p id=bad role=alert hidden>This link is missing its invite code. Ask for a new link.</p>
<p class=small>Nothing happens? Open the Commons app and paste this page's address: in <b>Add a server account</b> to create an account, or <b>Join with an invite link</b> if you already have one on this server.</p>
</main>
<script>{script}</script>
</html>
"#;

const LANDING_SCRIPT: &str = r##"
const m=location.pathname.match(/\/invite\/([^/]+)\/?$/);
const token=location.hash.slice(1)||(m?decodeURIComponent(m[1]):"");
const open=document.getElementById("open");
if(/^[0-9a-fA-F]{32}\.[A-Za-z0-9_-]{43}$/.test(token)){
 open.hidden=false;
 open.addEventListener("click",()=>{location.href="commons://invite?server="+
  encodeURIComponent(location.origin)+"#"+token;});
}else{document.getElementById("bad").hidden=false;}
"##;

#[cfg(test)]
mod tests {
    use super::mask_email;

    #[test]
    fn masked_emails_keep_the_domain_and_little_else() {
        let m = |e: &str| mask_email(e).replace('\u{2022}', "*");
        assert_eq!(m("adarsh@gmail.com"), "ad***@gmail.com");
        assert_eq!(m("abcd@x.org"), "ab***@x.org");
        assert_eq!(m("abc@x.org"), "a***@x.org");
        assert_eq!(m("ab@x.org"), "a***@x.org");
        assert_eq!(m("a@x.org"), "***@x.org");
        assert_eq!(m("élodie@x.org"), "él***@x.org");
        assert_eq!(m("a.b@c@x.org"), "a.***@x.org");
    }
}
