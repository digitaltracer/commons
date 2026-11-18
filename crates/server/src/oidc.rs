//! Sign-in through an OIDC provider (decision 14): server-side authorization code with PKCE.
//!
//! The app starts a flow and opens the provider in the system browser. The provider sends the
//! browser back to [`callback`], which verifies the id_token and redirects to the app's
//! loopback address with a short-lived handoff code. The app trades that code, plus the
//! verifier for the challenge it started with, for a session at [`exchange`].
use crate::sqlx;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use proto::api::{
    IdentityList, OidcExchange, OidcExchanged, OidcIdentity, OidcStart, OidcStarted,
    SessionResponse,
};
use proto::model::User;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthUser, ClientIp, hash_token, new_token};
use crate::config::{OidcConfig, Registration};
use crate::error::{ApiError, ApiResult};

const FLOW_MINUTES: i32 = 10;
const HANDOFF_SECONDS: i32 = 120;
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// An unknown key id refetches the JWKS at most this often.
const JWKS_REFETCH: Duration = Duration::from_secs(30);
const ASYMMETRIC: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/oidc/start", post(start))
        .route("/auth/oidc/callback", get(callback))
        .route("/auth/oidc/exchange", post(exchange))
        .route("/me/identities", get(identities))
        .route("/me/identities/{id}", delete(unlink))
}

/// The configured provider, with its discovery document and keys cached.
pub struct Provider {
    config: OidcConfig,
    http: reqwest::Client,
    cache: tokio::sync::Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    discovery: Option<(Discovery, Instant)>,
    jwks: Option<(JwkSet, Instant)>,
}

#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    id_token_signing_alg_values_supported: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
    /// A boolean, or the string "true" from some providers.
    #[serde(default)]
    email_verified: serde_json::Value,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    aud: serde_json::Value,
}

/// https, or http to a loopback host (for tests and local providers).
fn check_secure(raw: &str) -> anyhow::Result<()> {
    let u = url::Url::parse(raw)?;
    let loopback = match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d == "localhost",
        None => false,
    };
    anyhow::ensure!(
        u.scheme() == "https" || (u.scheme() == "http" && loopback),
        "{raw} must use https"
    );
    Ok(())
}

impl Provider {
    pub fn new(config: OidcConfig) -> anyhow::Result<Self> {
        check_secure(&config.issuer).map_err(|e| anyhow::anyhow!("COMMONS_OIDC_ISSUER: {e}"))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Provider {
            config,
            http,
            cache: Default::default(),
        })
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    async fn discovery(&self) -> anyhow::Result<Discovery> {
        let mut cache = self.cache.lock().await;
        if let Some((d, at)) = &cache.discovery
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(d.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", self.config.issuer);
        let d: Discovery = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::ensure!(
            d.issuer.trim_end_matches('/') == self.config.issuer,
            "the provider's issuer is {:?}, not the configured {:?}",
            d.issuer,
            self.config.issuer
        );
        for endpoint in [&d.authorization_endpoint, &d.token_endpoint, &d.jwks_uri] {
            check_secure(endpoint)?;
        }
        cache.discovery = Some((d.clone(), Instant::now()));
        Ok(d)
    }

    async fn key(&self, d: &Discovery, kid: Option<&str>) -> anyhow::Result<Jwk> {
        let find = |set: &JwkSet| match kid {
            Some(k) => set.find(k).cloned(),
            None if set.keys.len() == 1 => Some(set.keys[0].clone()),
            None => None,
        };
        let mut cache = self.cache.lock().await;
        if let Some((set, at)) = &cache.jwks {
            if let Some(k) = find(set) {
                return Ok(k);
            }
            anyhow::ensure!(
                at.elapsed() >= JWKS_REFETCH,
                "no signing key {kid:?} at the provider"
            );
        }
        let set: JwkSet = self
            .http
            .get(&d.jwks_uri)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let found = find(&set);
        cache.jwks = Some((set, Instant::now()));
        found.ok_or_else(|| anyhow::anyhow!("no signing key {kid:?} at the provider"))
    }

    /// Trades the provider's code for its id_token, proving the flow with the PKCE verifier.
    async fn redeem_code(
        &self,
        d: &Discovery,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Tokens {
            id_token: String,
        }
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        let methods = &d.token_endpoint_auth_methods_supported;
        let mut req = self.http.post(&d.token_endpoint);
        // The spec's default is client_secret_basic; prefer post, which needs no extra encoding.
        if methods.iter().any(|m| m == "client_secret_post") {
            form.push(("client_id", &self.config.client_id));
            form.push(("client_secret", &self.config.client_secret));
        } else {
            let enc =
                |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
            req = req.basic_auth(
                enc(&self.config.client_id),
                Some(enc(&self.config.client_secret)),
            );
        }
        let resp = req.form(&form).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("token endpoint answered {status}: {}", truncate(&body, 300));
        }
        Ok(resp.json::<Tokens>().await?.id_token)
    }

    /// Checks the id_token's signature against the provider's keys, and its issuer, audience,
    /// expiry and nonce.
    async fn verify(&self, d: &Discovery, id_token: &str, nonce: &str) -> anyhow::Result<Claims> {
        let head = jsonwebtoken::decode_header(id_token)?;
        anyhow::ensure!(
            ASYMMETRIC.contains(&head.alg),
            "unsupported id_token algorithm {:?}",
            head.alg
        );
        let supported = &d.id_token_signing_alg_values_supported;
        anyhow::ensure!(
            supported.is_empty() || supported.contains(&format!("{:?}", head.alg)),
            "the provider doesn't list {:?}",
            head.alg
        );
        let jwk = self.key(d, head.kid.as_deref()).await?;
        let key = DecodingKey::from_jwk(&jwk)?;
        let mut v = Validation::new(head.alg);
        v.set_issuer(&[&d.issuer]);
        v.set_audience(&[&self.config.client_id]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.leeway = 60;
        let claims = jsonwebtoken::decode::<Claims>(id_token, &key, &v)?.claims;
        anyhow::ensure!(
            claims
                .nonce
                .as_deref()
                .is_some_and(|n| constant_time_eq(n.as_bytes(), nonce.as_bytes())),
            "the id_token's nonce doesn't match"
        );
        if claims.aud.as_array().is_some_and(|a| a.len() > 1) {
            anyhow::ensure!(
                claims.azp.as_deref() == Some(self.config.client_id.as_str()),
                "the id_token has several audiences and isn't authorized for this client"
            );
        }
        Ok(claims)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y));
    std::hint::black_box(diff) == 0
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn redirect_uri(state: &AppState) -> String {
    format!(
        "{}/v1/auth/oidc/callback",
        state.config.public_url.trim_end_matches('/')
    )
}

fn provider(state: &AppState) -> ApiResult<&Provider> {
    state.oidc.as_ref().ok_or(ApiError::NotFound)
}

/// Only loopback http targets, per RFC 8252: the app listens on 127.0.0.1 or [::1].
fn check_return_to(raw: &str) -> ApiResult<url::Url> {
    let bad = || ApiError::BadRequest("return_to must be http://127.0.0.1:<port>/…".into());
    let u = url::Url::parse(raw).map_err(|_| bad())?;
    let loopback = match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if u.scheme() != "http"
        || !loopback
        || u.port().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
    {
        return Err(bad());
    }
    Ok(u)
}

fn check_challenge(c: &str) -> ApiResult<()> {
    let ok = c.len() == 43
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(ApiError::BadRequest(
            "code_challenge must be an S256 PKCE challenge".into(),
        ));
    }
    Ok(())
}

#[utoipa::path(post, path = "/v1/auth/oidc/start", request_body = OidcStart,
    responses(
        (status = 200, body = OidcStarted),
        (status = 400), (status = 401, description = "link without a session"),
        (status = 404, description = "OIDC is off"), (status = 429),
        (status = 503, description = "the provider didn't answer"),
    ))]
pub async fn start(
    State(state): State<AppState>,
    ip: ClientIp,
    auth: Result<AuthUser, ApiError>,
    Json(req): Json<OidcStart>,
) -> ApiResult<Json<OidcStarted>> {
    ip.check(&state, "oidc")?;
    let provider = provider(&state)?;
    check_return_to(&req.return_to)?;
    check_challenge(&req.code_challenge)?;
    let invite = req.invite.filter(|t| !t.trim().is_empty());
    let link_user = match req.link {
        true if invite.is_some() => {
            return Err(ApiError::BadRequest(
                "an invite can't be used to link".into(),
            ));
        }
        true => Some(auth?.user_id),
        false => None,
    };
    let d = provider.discovery().await.map_err(|e| {
        tracing::warn!(error = ?e, "OIDC discovery failed");
        ApiError::Unavailable(format!(
            "{} isn't answering. Try again later.",
            provider.name()
        ))
    })?;
    let flow_state = new_token();
    let nonce = new_token();
    let verifier = new_token();
    sqlx::query("DELETE FROM oidc_flows WHERE expires_at < now()")
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM oidc_handoffs WHERE expires_at < now()")
        .execute(&state.pool)
        .await?;
    sqlx::query(
        "INSERT INTO oidc_flows (state_hash, nonce, pkce_verifier, return_to, app_challenge,
                                 invite, link_user_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(mins => $8))",
    )
    .bind(&hash_token(&flow_state)[..])
    .bind(&nonce)
    .bind(&verifier)
    .bind(&req.return_to)
    .bind(&req.code_challenge)
    .bind(&invite)
    .bind(link_user)
    .bind(FLOW_MINUTES)
    .execute(&state.pool)
    .await?;
    let mut url = url::Url::parse(&d.authorization_endpoint)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("authorization_endpoint: {e}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &provider.config.client_id)
        .append_pair("redirect_uri", &redirect_uri(&state))
        .append_pair("scope", "openid email profile")
        .append_pair("state", &flow_state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");
    Ok(Json(OidcStarted {
        authorize_url: url.into(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

struct Flow {
    nonce: String,
    pkce_verifier: String,
    return_to: String,
    app_challenge: String,
    invite: Option<String>,
    link_user_id: Option<Uuid>,
}

/// A refusal shown to the person in the app: a stable code and a sentence.
struct Refusal(&'static str, String);

impl From<ApiError> for Refusal {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Refused(code, message) => Refusal(code, message),
            ApiError::Conflict(v) => Refusal(
                "conflict",
                v["message"].as_str().unwrap_or("conflict").to_string(),
            ),
            other => {
                tracing::error!(error = ?other, "OIDC callback failed");
                Refusal("internal", "Something went wrong on the server.".into())
            }
        }
    }
}

impl From<sqlx::Error> for Refusal {
    fn from(e: sqlx::Error) -> Self {
        ApiError::from(e).into()
    }
}

fn page(status: StatusCode, title: &str, text: &str) -> Response {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{t}</title><h1>{t}</h1><p>{p}</p>",
        t = esc(title),
        p = esc(text)
    );
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(body),
    )
        .into_response()
}

fn redirect_to(target: url::Url) -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, target.to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
        ],
    )
        .into_response()
}

/// Where the provider sends the browser back. Every outcome with a known flow redirects to
/// the app; an unknown, used or expired state gets a page, because there's nowhere to go.
#[utoipa::path(get, path = "/v1/auth/oidc/callback",
    responses((status = 303, description = "back to the app"), (status = 400)))]
pub async fn callback(
    State(state): State<AppState>,
    ip: ClientIp,
    Query(q): Query<CallbackQuery>,
) -> Response {
    if ip.check(&state, "oidc").is_err() {
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts",
            "Wait a minute, then start again from the Commons app.",
        );
    }
    let Some(provider) = state.oidc.as_ref() else {
        return page(
            StatusCode::NOT_FOUND,
            "Not found",
            "Sign-in with a provider is off.",
        );
    };
    let stale = || {
        page(
            StatusCode::BAD_REQUEST,
            "This sign-in has expired",
            "It was already used, timed out or didn't start here. Start again from the Commons app.",
        )
    };
    let Some(raw_state) = q.state.as_deref() else {
        return stale();
    };
    let flow = sqlx::query_as::<_, (String, String, String, String, Option<String>, Option<Uuid>)>(
        "DELETE FROM oidc_flows WHERE state_hash = $1 AND expires_at > now()
         RETURNING nonce, pkce_verifier, return_to, app_challenge, invite, link_user_id",
    )
    .bind(&hash_token(raw_state)[..])
    .fetch_optional(&state.pool)
    .await;
    let flow = match flow {
        Ok(Some((nonce, pkce_verifier, return_to, app_challenge, invite, link_user_id))) => Flow {
            nonce,
            pkce_verifier,
            return_to,
            app_challenge,
            invite,
            link_user_id,
        },
        Ok(None) => return stale(),
        Err(e) => {
            tracing::error!(error = ?e, "OIDC flow lookup failed");
            return page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Start again from the Commons app.",
            );
        }
    };
    let Ok(mut target) = url::Url::parse(&flow.return_to) else {
        return stale();
    };
    match finish(&state, provider, &flow, &q).await {
        Ok(code) => {
            target.query_pairs_mut().append_pair("code", &code);
        }
        Err(Refusal(code, message)) => {
            tracing::info!(code, "OIDC sign-in refused");
            target
                .query_pairs_mut()
                .append_pair("error", code)
                .append_pair("message", &message);
        }
    }
    redirect_to(target)
}

enum Kind {
    Session,
    Linked,
}

/// Verifies the provider's answer, resolves the account and returns a handoff code.
async fn finish(
    state: &AppState,
    provider: &Provider,
    flow: &Flow,
    q: &CallbackQuery,
) -> Result<String, Refusal> {
    let name = provider.name();
    if let Some(err) = &q.error {
        let detail = q.error_description.as_deref().unwrap_or(err);
        return Err(Refusal(
            "provider_refused",
            format!("{name} didn't sign you in: {}", truncate(detail, 200)),
        ));
    }
    let code = q
        .code
        .as_deref()
        .ok_or_else(|| Refusal("provider_refused", format!("{name} sent no sign-in code.")))?;
    let d = provider.discovery().await.map_err(|e| {
        tracing::warn!(error = ?e, "OIDC discovery failed");
        Refusal(
            "provider_unavailable",
            format!("{name} isn't answering. Try again later."),
        )
    })?;
    let id_token = provider
        .redeem_code(&d, code, &flow.pkce_verifier, &redirect_uri(state))
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, "OIDC code exchange failed");
            Refusal(
                "provider_refused",
                format!("{name} didn't accept this sign-in. Try again."),
            )
        })?;
    let claims = provider
        .verify(&d, &id_token, &flow.nonce)
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, "id_token rejected");
            Refusal(
                "token_invalid",
                format!("The sign-in from {name} couldn't be verified."),
            )
        })?;
    let (user_id, kind) = match flow.link_user_id {
        Some(user_id) => {
            check_link(state, name, &d.issuer, &claims, user_id).await?;
            (user_id, Kind::Linked)
        }
        None => (
            sign_in(state, name, &d.issuer, &claims, flow).await?,
            Kind::Session,
        ),
    };
    // A link is only recorded here and made at the exchange, so a link flow someone else
    // started can't attach this person's identity to their account.
    let linked = matches!(kind, Kind::Linked);
    let handoff = new_token();
    sqlx::query(
        "INSERT INTO oidc_handoffs (code_hash, user_id, kind, app_challenge, expires_at,
                                    issuer, subject, email, email_verified)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5), $6, $7, $8, $9)",
    )
    .bind(&hash_token(&handoff)[..])
    .bind(user_id)
    .bind(if linked { "linked" } else { "session" })
    .bind(&flow.app_challenge)
    .bind(HANDOFF_SECONDS as f64)
    .bind(linked.then_some(&d.issuer))
    .bind(linked.then_some(&claims.sub))
    .bind(claims.email.as_ref().filter(|_| linked))
    .bind(linked && email_verified(&claims.email_verified))
    .execute(&state.pool)
    .await?;
    Ok(handoff)
}

fn email_verified(v: &serde_json::Value) -> bool {
    v.as_bool() == Some(true) || v.as_str() == Some("true")
}

async fn is_deactivated(conn: &mut sqlx::PgConnection, user_id: Uuid) -> Result<bool, Refusal> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT deactivated_at IS NOT NULL FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(true),
    )
}

fn deactivated() -> Refusal {
    Refusal(
        "account_deactivated",
        "This account is deactivated on this server.".into(),
    )
}

/// The account for this identity, made if there is none. An identity never joins an existing
/// account by email; that takes an explicit link from the signed-in account.
async fn sign_in(
    state: &AppState,
    name: &str,
    issuer: &str,
    claims: &Claims,
    flow: &Flow,
) -> Result<Uuid, Refusal> {
    let verified = email_verified(&claims.email_verified);
    let mut tx = state.pool.begin().await?;
    let found: Option<Uuid> = sqlx::query_scalar(
        "UPDATE oidc_identities SET last_used_at = now(), email = $3, email_verified = $4
          WHERE issuer = $1 AND subject = $2 RETURNING user_id",
    )
    .bind(issuer)
    .bind(&claims.sub)
    .bind(&claims.email)
    .bind(verified)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(user_id) = found {
        if is_deactivated(&mut tx, user_id).await? {
            return Err(deactivated());
        }
        if let Some(raw) = &flow.invite {
            let invite = crate::invites::prepare(&mut tx, raw).await?;
            let member = crate::sync::active_role(&mut tx, invite.workspace.id, user_id)
                .await
                .map_err(Refusal::from)?;
            if member.is_none() {
                crate::invites::redeem(&mut tx, &invite, user_id).await?;
            }
        }
        tx.commit().await?;
        return Ok(user_id);
    }

    if state.config.registration == Registration::Closed {
        return Err(Refusal(
            "registration_closed",
            "registration is closed on this server".into(),
        ));
    }
    let invite = match &flow.invite {
        Some(raw) => Some(crate::invites::prepare(&mut tx, raw).await?),
        None => None,
    };
    if invite.is_none() && state.config.registration == Registration::Invite {
        crate::auth::check_bootstrap(&mut tx).await?;
    }
    let email = claims
        .email
        .as_deref()
        .ok_or_else(|| {
            Refusal(
                "email_missing",
                format!("{name} didn't share an email address, which a new account needs."),
            )
        })
        .and_then(|e| {
            crate::auth::normalise_email(e).map_err(|_| {
                Refusal(
                    "email_missing",
                    format!("{name} shared an email address this server can't use."),
                )
            })
        })?;
    let taken = || {
        Refusal(
            "email_taken",
            format!(
                "An account with {email} already exists. Sign in with its password, then link \
                 {name} in Settings."
            ),
        )
    };
    let display = claims
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("New member"));
    let display = truncate(display, 80).to_string();
    let user_id = Uuid::now_v7();
    let inserted = sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name) VALUES ($1, $2, NULL, $3)
         ON CONFLICT (email) DO NOTHING",
    )
    .bind(user_id)
    .bind(&email)
    .bind(&display)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(taken());
    }
    insert_identity(
        &mut tx,
        user_id,
        issuer,
        &claims.sub,
        claims.email.as_deref(),
        verified,
    )
    .await?;
    if let Some(invite) = &invite {
        crate::invites::redeem(&mut tx, invite, user_id).await?;
    }
    tx.commit().await?;
    tracing::info!(%user_id, "account created through OIDC");
    Ok(user_id)
}

async fn insert_identity(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
    verified: bool,
) -> ApiResult<()> {
    let inserted = sqlx::query(
        "INSERT INTO oidc_identities (id, user_id, issuer, subject, email, email_verified,
                                      last_used_at)
         VALUES ($1, $2, $3, $4, $5, $6, now()) ON CONFLICT (issuer, subject) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(issuer)
    .bind(subject)
    .bind(email)
    .bind(verified)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(ApiError::Refused(
            "identity_in_use",
            "That sign-in is already linked to an account.".into(),
        ));
    }
    Ok(())
}

/// Checks that the identity can be linked to the account that started the flow, so the
/// person hears now rather than after the browser hands back.
async fn check_link(
    state: &AppState,
    name: &str,
    issuer: &str,
    claims: &Claims,
    user_id: Uuid,
) -> Result<(), Refusal> {
    let mut conn = state.pool.acquire().await?;
    if is_deactivated(&mut conn, user_id).await? {
        return Err(deactivated());
    }
    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM oidc_identities WHERE issuer = $1 AND subject = $2",
    )
    .bind(issuer)
    .bind(&claims.sub)
    .fetch_optional(&mut *conn)
    .await?;
    match owner {
        Some(u) if u != user_id => Err(Refusal(
            "identity_in_use",
            format!("That {name} account already signs in to another Commons account."),
        )),
        _ => Ok(()),
    }
}

#[utoipa::path(post, path = "/v1/auth/oidc/exchange", request_body = OidcExchange,
    responses((status = 200, body = OidcExchanged), (status = 400), (status = 403), (status = 429)))]
pub async fn exchange(
    State(state): State<AppState>,
    ip: ClientIp,
    auth: Result<AuthUser, ApiError>,
    Json(req): Json<OidcExchange>,
) -> ApiResult<Json<OidcExchanged>> {
    ip.check(&state, "oidc-exchange")?;
    let invalid = || ApiError::BadRequest("this sign-in code is invalid or expired".into());
    let mut tx = state.pool.begin().await?;
    type Handoff = (
        Uuid,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
    );
    let (user_id, kind, challenge, issuer, subject, link_email, verified): Handoff =
        sqlx::query_as(
            "DELETE FROM oidc_handoffs WHERE code_hash = $1 AND expires_at > now()
             RETURNING user_id, kind, app_challenge, issuer, subject, email, email_verified",
        )
        .bind(&hash_token(&req.code)[..])
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(invalid)?;
    if !constant_time_eq(
        pkce_challenge(&req.code_verifier).as_bytes(),
        challenge.as_bytes(),
    ) {
        // The code is spent either way.
        tx.commit().await?;
        return Err(invalid());
    }
    let (email, display_name, deactivated): (String, String, bool) = sqlx::query_as(
        "SELECT email, display_name, deactivated_at IS NOT NULL FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if deactivated {
        tx.commit().await?;
        return Err(ApiError::Refused(
            "account_deactivated",
            "This account is deactivated on this server.".into(),
        ));
    }
    let out = if kind == "session" {
        let token = crate::auth::create_session(&mut tx, user_id).await?;
        OidcExchanged::Session {
            session: SessionResponse {
                token,
                user: User {
                    id: user_id,
                    email,
                    display_name,
                },
            },
        }
    } else {
        // A link also needs the session that started it.
        if auth?.user_id != user_id {
            tx.commit().await?;
            return Err(ApiError::Forbidden);
        }
        let (Some(issuer), Some(subject)) = (issuer, subject) else {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "link handoff without identity"
            )));
        };
        let mine: Option<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM oidc_identities WHERE issuer = $1 AND subject = $2",
        )
        .bind(&issuer)
        .bind(&subject)
        .fetch_optional(&mut *tx)
        .await?;
        if mine != Some(user_id) {
            insert_identity(
                &mut tx,
                user_id,
                &issuer,
                &subject,
                link_email.as_deref(),
                verified,
            )
            .await?;
            tracing::info!(%user_id, "OIDC identity linked");
        }
        OidcExchanged::Linked {
            identities: list_identities(&mut tx, user_id).await?,
        }
    };
    tx.commit().await?;
    Ok(Json(out))
}

async fn list_identities(conn: &mut sqlx::PgConnection, user_id: Uuid) -> ApiResult<IdentityList> {
    let has_password: bool =
        sqlx::query_scalar("SELECT password_hash IS NOT NULL FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&mut *conn)
            .await?;
    let rows: Vec<(Uuid, String, Option<String>, time::OffsetDateTime)> = sqlx::query_as(
        "SELECT id, issuer, email, created_at FROM oidc_identities WHERE user_id = $1
          ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(IdentityList {
        has_password,
        identities: rows
            .into_iter()
            .map(|(id, issuer, email, at)| OidcIdentity {
                id,
                issuer,
                email,
                created_at_ms: crate::db::ms(at),
            })
            .collect(),
    })
}

#[utoipa::path(get, path = "/v1/me/identities", responses((status = 200, body = IdentityList)))]
pub async fn identities(
    State(state): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<IdentityList>> {
    let mut conn = state.pool.acquire().await?;
    Ok(Json(list_identities(&mut conn, auth.user_id).await?))
}

/// Unlinks an identity, unless it is the account's only way to sign in.
#[utoipa::path(delete, path = "/v1/me/identities/{id}", params(("id" = Uuid, Path)),
    responses((status = 204), (status = 403, description = "the last way to sign in"),
        (status = 404)))]
pub async fn unlink(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    // The row lock orders concurrent unlinks, so two can't each leave the other as the last.
    let has_password: bool =
        sqlx::query_scalar("SELECT password_hash IS NOT NULL FROM users WHERE id = $1 FOR UPDATE")
            .bind(auth.user_id)
            .fetch_one(&mut *tx)
            .await?;
    let others: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oidc_identities WHERE user_id = $1 AND id <> $2")
            .bind(auth.user_id)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let found: Option<bool> =
        sqlx::query_scalar("SELECT true FROM oidc_identities WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(auth.user_id)
            .fetch_optional(&mut *tx)
            .await?;
    found.ok_or(ApiError::NotFound)?;
    if !has_password && others == 0 {
        return Err(ApiError::Refused(
            "last_sign_in_method",
            "This is the only way this account signs in, so it can't be unlinked.".into(),
        ));
    }
    sqlx::query("DELETE FROM oidc_identities WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(user_id = %auth.user_id, identity = %id, "OIDC identity unlinked");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_targets_are_loopback_http_only() {
        assert!(check_return_to("http://127.0.0.1:53111/oidc").is_ok());
        assert!(check_return_to("http://[::1]:53111/oidc").is_ok());
        for bad in [
            "https://127.0.0.1:53111/oidc",
            "http://localhost:53111/oidc",
            "http://127.0.0.1/oidc",
            "http://evil.example:53111/oidc",
            "http://user@127.0.0.1:53111/oidc",
            "http://127.0.0.1:53111/oidc#x",
            "commons://oidc",
        ] {
            assert!(check_return_to(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn issuers_need_https_except_on_loopback() {
        assert!(check_secure("https://accounts.google.com").is_ok());
        assert!(check_secure("http://127.0.0.1:9000").is_ok());
        assert!(check_secure("http://accounts.example.com").is_err());
    }
}
