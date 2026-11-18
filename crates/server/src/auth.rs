//! Email/password accounts and opaque, revocable sessions (decision 14).
use crate::sqlx;
use std::num::NonZeroU32;

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use proto::api::{LoginRequest, RegisterRequest, ResetConfirm, ResetRequest, SessionResponse};
use proto::model::User;
use rand::RngCore;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppState;
use crate::config::Registration;
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/reset", post(reset_request))
        .route("/auth/reset/confirm", post(reset_confirm))
        .route("/me", get(me))
}

/// Attempt limiter for login and reset, keyed by email or by client IP.
pub struct Limiter(DefaultKeyedRateLimiter<String>);

impl Limiter {
    pub fn new(per_minute: u32) -> Self {
        let n = NonZeroU32::new(per_minute.max(1)).unwrap_or(NonZeroU32::MIN);
        Limiter(RateLimiter::keyed(Quota::per_minute(n)))
    }
    pub fn check(&self, key: &str) -> ApiResult<()> {
        self.0
            .check_key(&key.to_string())
            .map_err(|_| ApiError::RateLimited)
    }
    /// Drops keys whose limit has fully replenished, so unique keys can't grow memory.
    pub fn prune(&self) {
        self.0.retain_recent();
        self.0.shrink_to_fit();
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub async fn prune_limiters(state: AppState, cancel: tokio_util::sync::CancellationToken) {
    let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = every.tick() => {
                state.limiter.prune();
                state.ip_limiter.prune();
            }
        }
    }
}

/// The caller's address: the peer, or the address a trusted proxy (loopback or listed)
/// forwarded in `X-Real-IP`, else the last `X-Forwarded-For` entry.
pub fn client_ip(
    peer: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
    trusted: &[std::net::IpAddr],
) -> std::net::IpAddr {
    if !(peer.is_loopback() || trusted.contains(&peer)) {
        return peer;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("x-real-ip")
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| {
            header("x-forwarded-for")
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(peer)
}

/// The caller's IP, when the server runs with connect info.
pub struct ClientIp(pub Option<std::net::IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip());
        Ok(ClientIp(peer.map(|p| {
            client_ip(p, &parts.headers, &state.config.trusted_proxies)
        })))
    }
}

impl ClientIp {
    pub(crate) fn check(&self, state: &AppState, action: &str) -> ApiResult<()> {
        match self.0 {
            Some(ip) => state.ip_limiter.check(&format!("{action}:{ip}")),
            None => Ok(()),
        }
    }
}

/// The signed-in caller, from `Authorization: Bearer <token>`.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub user_id: Uuid,
    /// The session's token hash, so sockets opened with it close when it is revoked.
    pub session: [u8; 32],
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(ApiError::Unauthorized)?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or(ApiError::Unauthorized)?;
        let session = hash_token(token);
        let user_id = session_user(&state.pool, &session)
            .await?
            .ok_or(ApiError::Unauthorized)?;
        Ok(AuthUser { user_id, session })
    }
}

pub async fn session_user(pool: &sqlx::PgPool, hash: &[u8; 32]) -> ApiResult<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT s.user_id FROM sessions s JOIN users u ON u.id = s.user_id
          WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND u.deactivated_at IS NULL",
    )
    .bind(&hash[..])
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Revokes every session of `user_id` and closes their sockets once the caller commits.
pub async fn revoke_user_sessions(conn: &mut sqlx::PgConnection, user_id: Uuid) -> ApiResult<u64> {
    let sql = format!(
        "WITH r AS (UPDATE sessions SET revoked_at = now()
                     WHERE user_id = $1 AND revoked_at IS NULL RETURNING token_hash)
         {NOTIFY_REVOKED}"
    );
    let rows = sqlx::query(&sql)
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.len() as u64)
}

/// Tells every node to close sockets of the sessions in `r`; delivered only on commit.
pub(crate) const NOTIFY_REVOKED: &str =
    "SELECT pg_notify('commons', 's:' || encode(r.token_hash, 'hex')) FROM r";

pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn hash_password(password: &str) -> ApiResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("hash: {e}")))
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
        .unwrap_or(false)
}

pub(crate) fn normalise_email(email: &str) -> ApiResult<String> {
    let e = email.trim().to_lowercase();
    if e.len() < 3 || !e.contains('@') || e.len() > 254 {
        return Err(ApiError::BadRequest("invalid email".into()));
    }
    Ok(e)
}

/// In invite mode, an account without an invite is allowed only as the server's first, so a
/// fresh install can start. Concurrent first registrations are serialized.
pub(crate) async fn check_bootstrap(conn: &mut sqlx::PgConnection) -> ApiResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('commons:first-account', 0))")
        .execute(&mut *conn)
        .await?;
    let empty: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM users)")
        .fetch_one(&mut *conn)
        .await?;
    if !empty {
        return Err(ApiError::Refused(
            "invite_required",
            "this server needs an invite link to create an account".into(),
        ));
    }
    Ok(())
}

fn check_password(p: &str) -> ApiResult<()> {
    if p.chars().count() < 8 || p.len() > 1024 {
        return Err(ApiError::BadRequest(
            "password must be at least 8 characters".into(),
        ));
    }
    Ok(())
}

pub(crate) async fn create_session(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
) -> ApiResult<String> {
    let token = new_token();
    sqlx::query("INSERT INTO sessions (token_hash, user_id) VALUES ($1, $2)")
        .bind(&hash_token(&token)[..])
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    Ok(token)
}

#[utoipa::path(post, path = "/v1/auth/register", request_body = RegisterRequest,
    responses(
        (status = 200, body = SessionResponse),
        (status = 429),
        (status = 403, description = "registration is closed, or an invite is required, \
            invalid, expired, used or revoked", body = proto::api::ErrorBody),
    ))]
pub async fn register(
    State(state): State<AppState>,
    ip: ClientIp,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<SessionResponse>> {
    ip.check(&state, "register")?;
    let email = normalise_email(&req.email)?;
    if state.config.registration == Registration::Closed {
        return Err(ApiError::Refused(
            "registration_closed",
            "registration is closed on this server".into(),
        ));
    }
    check_password(&req.password)?;
    let name = req.display_name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err(ApiError::BadRequest("display name is required".into()));
    }
    let id = Uuid::now_v7();
    // One transaction: the account, its session and, with an invite, its workspace. The
    // invite or first-account rule is checked before the costly hash, under the same locks.
    let mut tx = state.pool.begin().await?;
    let invite = match req.invite.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(raw) => Some(crate::invites::prepare(&mut tx, raw).await?),
        None => None,
    };
    if invite.is_none() && state.config.registration == Registration::Invite {
        check_bootstrap(&mut tx).await?;
    }
    let password = req.password.clone();
    state
        .password_hashes
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(e.into()))??;
    let inserted = sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name) VALUES ($1, $2, $3, $4)
         ON CONFLICT (email) DO NOTHING",
    )
    .bind(id)
    .bind(&email)
    .bind(&hash)
    .bind(name)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(ApiError::Conflict(serde_json::json!({
            "error": "email_taken", "message": "an account with this email exists"
        })));
    }
    let token = create_session(&mut tx, id).await?;
    if let Some(invite) = &invite {
        crate::invites::redeem(&mut tx, invite, id).await?;
    }
    tx.commit().await?;
    Ok(Json(SessionResponse {
        token,
        user: User {
            id,
            email,
            display_name: name.to_string(),
        },
    }))
}

#[utoipa::path(post, path = "/v1/auth/login", request_body = LoginRequest,
    responses((status = 200, body = SessionResponse), (status = 401), (status = 429)))]
pub async fn login(
    State(state): State<AppState>,
    ip: ClientIp,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<SessionResponse>> {
    ip.check(&state, "login")?;
    let email = normalise_email(&req.email)?;
    state.limiter.check(&format!("login:{email}"))?;
    // Accounts without a password (made through OIDC) and deactivated ones can't sign in here.
    let row: Option<(Uuid, Option<String>, String)> = sqlx::query_as(
        "SELECT id, password_hash, display_name FROM users
          WHERE email = $1 AND deactivated_at IS NULL",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;
    let Some((id, Some(hash), display_name)) = row else {
        return Err(ApiError::Unauthorized);
    };
    let password = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    if !ok {
        return Err(ApiError::Unauthorized);
    }
    let token = create_session(&mut *state.pool.acquire().await?, id).await?;
    Ok(Json(SessionResponse {
        token,
        user: User {
            id,
            email,
            display_name,
        },
    }))
}

#[utoipa::path(post, path = "/v1/auth/logout", responses((status = 204)))]
pub async fn logout(
    State(state): State<AppState>,
    parts: axum::http::HeaderMap,
) -> ApiResult<StatusCode> {
    if let Some(token) = parts
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        let sql = format!(
            "WITH r AS (UPDATE sessions SET revoked_at = now()
                         WHERE token_hash = $1 AND revoked_at IS NULL RETURNING token_hash)
             {NOTIFY_REVOKED}"
        );
        sqlx::query(&sql)
            .bind(&hash_token(token)[..])
            .execute(&state.pool)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/v1/me", responses((status = 200, body = User)))]
pub async fn me(State(state): State<AppState>, auth: AuthUser) -> ApiResult<Json<User>> {
    let (id, email, display_name): (Uuid, String, String) =
        sqlx::query_as("SELECT id, email, display_name FROM users WHERE id = $1")
            .bind(auth.user_id)
            .fetch_one(&state.pool)
            .await?;
    Ok(Json(User {
        id,
        email,
        display_name,
    }))
}

/// Always answers 202 so the endpoint does not reveal which emails have accounts.
#[utoipa::path(post, path = "/v1/auth/reset", request_body = ResetRequest, responses((status = 202)))]
pub async fn reset_request(
    State(state): State<AppState>,
    ip: ClientIp,
    Json(req): Json<ResetRequest>,
) -> ApiResult<StatusCode> {
    ip.check(&state, "reset")?;
    let email = normalise_email(&req.email)?;
    state.limiter.check(&format!("reset:{email}"))?;
    let row: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&state.pool)
        .await?;
    if let Some((user_id,)) = row {
        let token = new_token();
        sqlx::query(
            "INSERT INTO password_resets (token_hash, user_id, expires_at)
             VALUES ($1, $2, now() + interval '1 hour')",
        )
        .bind(&hash_token(&token)[..])
        .bind(user_id)
        .execute(&state.pool)
        .await?;
        let link = format!("{}/reset?token={token}", state.config.public_url);
        let body = format!("Use this link within an hour to set a new password:\n\n{link}\n");
        if let Err(e) = state
            .mailer
            .send(&email, "Reset your Commons password", &body)
            .await
        {
            tracing::warn!(error = ?e, "reset mail failed");
        }
    }
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(post, path = "/v1/auth/reset/confirm", request_body = ResetConfirm,
    responses((status = 204), (status = 400)))]
pub async fn reset_confirm(
    State(state): State<AppState>,
    ip: ClientIp,
    Json(req): Json<ResetConfirm>,
) -> ApiResult<StatusCode> {
    ip.check(&state, "reset-confirm")?;
    check_password(&req.password)?;
    state
        .limiter
        .check(&format!("reset-confirm:{}", req.token))?;
    let mut tx = state.pool.begin().await?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE password_resets SET used_at = now()
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() RETURNING user_id",
    )
    .bind(&hash_token(&req.token)[..])
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id,)) = row else {
        return Err(ApiError::BadRequest(
            "reset link is invalid or expired".into(),
        ));
    };
    let password = req.password.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(e.into()))??;
    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
        .bind(hash)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "WITH r AS (UPDATE sessions SET revoked_at = now()
                     WHERE user_id = $1 AND revoked_at IS NULL RETURNING token_hash)
         {NOTIFY_REVOKED}"
    );
    sqlx::query(&sql).bind(user_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Where a reset link opens: a form that sets the new password. The token rides in the
/// fragment (`/reset#…`), so it never reaches a request line or access log.
pub async fn reset_page() -> impl axum::response::IntoResponse {
    (
        [
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::REFERRER_POLICY, "no-referrer"),
        ],
        axum::response::Html(RESET_PAGE),
    )
}

const RESET_PAGE: &str = r#"<!doctype html><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>Reset your Commons password</title>
<style>body{font:16px system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem}
label,input,button{display:block;width:100%;box-sizing:border-box}input{margin:.25rem 0 1rem;
padding:.5rem;font:inherit}button{padding:.6rem;font:inherit}#msg{margin-top:1rem}</style>
<h1>Set a new password</h1>
<form id=f><label for=p1>New password (at least 8 characters)</label>
<input id=p1 type=password autocomplete=new-password minlength=8 required>
<label for=p2>Repeat it</label>
<input id=p2 type=password autocomplete=new-password minlength=8 required>
<button>Set password</button></form><p id=msg role=status></p>
<script>
const q=new URLSearchParams(location.search);
const token=location.hash.slice(1)||q.get("token")||"";
history.replaceState(null,"",location.pathname);
const msg=document.getElementById("msg"),f=document.getElementById("f");
if(!token){f.hidden=true;msg.textContent="This link is missing its code. Ask for a new one.";}
f.onsubmit=async e=>{e.preventDefault();
 const p=document.getElementById("p1").value;
 if(p!==document.getElementById("p2").value){msg.textContent="The passwords don't match.";return;}
 const r=await fetch("/v1/auth/reset/confirm",{method:"POST",
  headers:{"content-type":"application/json"},body:JSON.stringify({token,password:p})});
 if(r.ok){f.hidden=true;msg.textContent="Done. Sign in to Commons with your new password.";}
 else{const b=await r.json().catch(()=>({}));
  msg.textContent=b.message||"That didn't work. The link may have expired.";}};
</script>
"#;
