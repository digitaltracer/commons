//! A fake OIDC provider for tests: discovery, an RS256 JWKS, an authorize endpoint that signs
//! the person in at once, and a token endpoint that enforces PKCE.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use uuid::Uuid;

use crate::config::OidcConfig;

pub const CLIENT_ID: &str = "commons-test-client";
pub const CLIENT_SECRET: &str = "test-client-secret";

/// What the next id_token says, and how it is spoiled.
#[derive(Clone)]
pub struct Next {
    pub sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub nonce: Option<String>,
    pub aud: Option<String>,
    pub exp_offset: i64,
}

impl Next {
    pub fn person(sub: &str, email: &str) -> Self {
        Next {
            sub: sub.into(),
            email: Some(email.into()),
            name: Some("Ada Lovelace".into()),
            nonce: None,
            aud: None,
            exp_offset: 600,
        }
    }
}

struct Pending {
    challenge: String,
    nonce: String,
    redirect_uri: String,
}

pub struct Fake {
    pub issuer: String,
    key: EncodingKey,
    jwk: Jwk,
    codes: Mutex<HashMap<String, Pending>>,
    /// What the next id_token says.
    pub next: Mutex<Next>,
}

impl Fake {
    /// The server settings that use this provider, named `name` on the button.
    pub fn config(&self, name: &str) -> OidcConfig {
        OidcConfig {
            issuer: self.issuer.clone(),
            client_id: CLIENT_ID.into(),
            client_secret: CLIENT_SECRET.into(),
            name: name.into(),
        }
    }

    pub fn set(&self, f: impl FnOnce(&mut Next)) {
        f(&mut self.next.lock().unwrap());
    }
}

/// The PKCS#1 key inside a PKCS#8 one, which is what jsonwebtoken's RSA signer takes.
fn pkcs1_from_pkcs8(der: &[u8]) -> Vec<u8> {
    fn header(d: &[u8]) -> (usize, usize) {
        let b = d[1] as usize;
        if b < 0x80 {
            return (2, b);
        }
        let n = b & 0x7f;
        let len = d[2..2 + n]
            .iter()
            .fold(0usize, |a, x| (a << 8) | *x as usize);
        (2 + n, len)
    }
    let (h, _) = header(der);
    let mut rest = &der[h..];
    for _ in 0..2 {
        let (h, len) = header(rest);
        rest = &rest[h + len..];
    }
    let (h, len) = header(rest);
    assert_eq!(rest[0], 0x04, "the private key octet string");
    rest[h..h + len].to_vec()
}

/// Starts the provider on an ephemeral loopback port.
pub async fn start() -> Arc<Fake> {
    use aws_lc_rs::encoding::AsDer;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let pair = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    let pkcs8 = pair.as_der().unwrap();
    let key = EncodingKey::from_rsa_der(&pkcs1_from_pkcs8(pkcs8.as_ref()));
    let mut jwk = Jwk::from_encoding_key(&key, Algorithm::RS256).unwrap();
    jwk.common.key_id = Some("test-key".into());
    let fake = Arc::new(Fake {
        issuer,
        key,
        jwk,
        codes: Mutex::default(),
        next: Mutex::new(Next::person("sub-1", "ada@example.org")),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    fake
}

async fn discovery(State(f): State<Arc<Fake>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "issuer": f.issuer,
        "authorization_endpoint": format!("{}/authorize", f.issuer),
        "token_endpoint": format!("{}/token", f.issuer),
        "jwks_uri": format!("{}/jwks", f.issuer),
        "id_token_signing_alg_values_supported": ["RS256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
    }))
}

async fn jwks(State(f): State<Arc<Fake>>) -> Json<JwkSet> {
    Json(JwkSet {
        keys: vec![f.jwk.clone()],
    })
}

/// Signs the person in at once and sends the browser back with a code.
async fn authorize(
    State(f): State<Arc<Fake>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["code_challenge_method"], "S256");
    let code = Uuid::now_v7().simple().to_string();
    f.codes.lock().unwrap().insert(
        code.clone(),
        Pending {
            challenge: q["code_challenge"].clone(),
            nonce: q["nonce"].clone(),
            redirect_uri: q["redirect_uri"].clone(),
        },
    );
    let mut back = url::Url::parse(&q["redirect_uri"]).unwrap();
    back.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &q["state"]);
    Redirect::to(back.as_str())
}

async fn token(
    State(f): State<Arc<Fake>>,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    let refuse = |why: &str| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_grant", "error_description": why})),
        )
            .into_response()
    };
    if form.get("client_id").map(String::as_str) != Some(CLIENT_ID)
        || form.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
    {
        return refuse("client");
    }
    let Some(p) = form
        .get("code")
        .and_then(|c| f.codes.lock().unwrap().remove(c))
    else {
        return refuse("unknown code");
    };
    if form.get("redirect_uri") != Some(&p.redirect_uri) {
        return refuse("redirect_uri");
    }
    let verifier = form.get("code_verifier").map(String::as_str).unwrap_or("");
    if crate::oidc::pkce_challenge(verifier) != p.challenge {
        return refuse("PKCE verifier");
    }
    let next = f.next.lock().unwrap().clone();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let claims = serde_json::json!({
        "iss": f.issuer,
        "sub": next.sub,
        "aud": next.aud.unwrap_or_else(|| CLIENT_ID.into()),
        "iat": now,
        "exp": now + next.exp_offset,
        "nonce": next.nonce.unwrap_or(p.nonce),
        "email": next.email,
        "email_verified": true,
        "name": next.name,
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key".into());
    let id_token = jsonwebtoken::encode(&header, &claims, &f.key).unwrap();
    Json(serde_json::json!({"access_token": "x", "token_type": "Bearer", "id_token": id_token}))
        .into_response()
}
