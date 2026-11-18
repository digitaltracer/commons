//! OIDC sign-in against a fake provider: discovery, JWKS and a token endpoint that enforces
//! PKCE, run in-process. Covers the flow's checks, the account rules and the app handoff.
mod common;

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use proto::api::{InviteCreated, OidcExchanged};
use reqwest::Method;
use server::config::{OidcConfig, Registration};
use server::fake_oidc::{CLIENT_ID, CLIENT_SECRET, Fake, Next};
use server::testing::TestServer;
use uuid::Uuid;

async fn fake_provider() -> Arc<Fake> {
    server::fake_oidc::start().await
}

async fn spawn(fake: &Fake, registration: Registration) -> TestServer {
    let oidc = fake.config("Google");
    server::testing::spawn(move |c| {
        c.registration = registration;
        c.oidc = Some(oidc);
    })
    .await
}

fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

const RETURN_TO: &str = "http://127.0.0.1:53111/oidc";

/// The app's side of the handoff: its verifier, kept until the exchange.
struct App {
    verifier: String,
}

impl App {
    fn new() -> Self {
        App {
            verifier: server::auth::new_token(),
        }
    }
}

/// Starts a flow and walks the browser to the provider; returns the callback URL it would
/// open next (the provider's code and the flow's state).
async fn to_provider(
    srv: &TestServer,
    app: &App,
    invite: Option<&str>,
    link_as: Option<&str>,
) -> String {
    let (s, v) = call(
        srv,
        Method::POST,
        link_as,
        "/auth/oidc/start",
        Some(serde_json::json!({
            "return_to": RETURN_TO,
            "code_challenge": server::oidc::pkce_challenge(&app.verifier),
            "invite": invite,
            "link": link_as.is_some(),
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let authorize = v["authorize_url"].as_str().unwrap();
    let resp = browser().get(authorize).send().await.unwrap();
    assert!(resp.status().is_redirection());
    let back = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(back.starts_with(&format!("{}/v1/auth/oidc/callback?", srv.base_url)));
    back
}

/// Where the callback sends the browser: the app's query (`code`, or `error` and `message`).
async fn back_to_app(callback: &str) -> Result<HashMap<String, String>, u16> {
    let resp = browser().get(callback).send().await.unwrap();
    if resp.status() != StatusCode::SEE_OTHER {
        return Err(resp.status().as_u16());
    }
    let target = url::Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
    assert!(target.as_str().starts_with(RETURN_TO), "{target}");
    Ok(target.query_pairs().into_owned().collect())
}

async fn exchange(srv: &TestServer, code: &str, verifier: &str) -> (u16, serde_json::Value) {
    exchange_as(srv, code, verifier, None).await
}

async fn exchange_as(
    srv: &TestServer,
    code: &str,
    verifier: &str,
    token: Option<&str>,
) -> (u16, serde_json::Value) {
    call(
        srv,
        Method::POST,
        token,
        "/auth/oidc/exchange",
        Some(serde_json::json!({"code": code, "code_verifier": verifier})),
    )
    .await
}

/// A whole sign-in: returns the session's (token, user id), or the refusal code.
async fn sign_in(
    srv: &TestServer,
    invite: Option<&str>,
) -> Result<(String, Uuid), (String, String)> {
    let app = App::new();
    let back = back_to_app(&to_provider(srv, &app, invite, None).await)
        .await
        .unwrap();
    if let Some(e) = back.get("error") {
        return Err((e.clone(), back["message"].clone()));
    }
    let (s, v) = exchange(srv, &back["code"], &app.verifier).await;
    assert_eq!(s, 200, "{v}");
    match serde_json::from_value::<OidcExchanged>(v).unwrap() {
        OidcExchanged::Session { session } => Ok((session.token, session.user.id)),
        other => panic!("{other:?}"),
    }
}

async fn identities(srv: &TestServer) -> i64 {
    server::sqlx::query_scalar("SELECT count(*) FROM oidc_identities")
        .fetch_one(srv.pool())
        .await
        .unwrap()
}

async fn users(srv: &TestServer) -> i64 {
    server::sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(srv.pool())
        .await
        .unwrap()
}

fn set_next(fake: &Fake, f: impl FnOnce(&mut Next)) {
    fake.set(f);
}

#[tokio::test(flavor = "multi_thread")]
async fn meta_advertises_the_provider_only_when_configured() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let (_, v) = call(&srv, Method::GET, None, "/meta", None).await;
    assert_eq!(v["oidc"]["name"], "Google");
    let off = server::testing::spawn(|_| {}).await;
    let (_, v) = call(&off, Method::GET, None, "/meta", None).await;
    assert!(v.get("oidc").is_none(), "{v}");
    let (s, _) = call(
        &off,
        Method::POST,
        None,
        "/auth/oidc/start",
        Some(serde_json::json!({"return_to": RETURN_TO, "code_challenge": "x".repeat(43)})),
    )
    .await;
    assert_eq!(s, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_creates_the_account_once_and_signs_it_in_again() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let (token, id) = sign_in(&srv, None).await.unwrap();
    let (s, me) = call(&srv, Method::GET, Some(&token), "/me", None).await;
    assert_eq!(s, 200);
    assert_eq!(me["id"], id.to_string());
    assert_eq!(me["email"], "ada@example.org");
    assert_eq!(me["display_name"], "Ada Lovelace");
    let verified: bool = server::sqlx::query_scalar(
        "SELECT email_verified FROM oidc_identities WHERE subject = 'sub-1'",
    )
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert!(verified, "email_verified is stored");

    // No password: password login refuses it.
    let (s, _) = call(
        &srv,
        Method::POST,
        None,
        "/auth/login",
        Some(serde_json::json!({"email": "ada@example.org", "password": "anything at all"})),
    )
    .await;
    assert_eq!(s, 401);

    let (_, again) = sign_in(&srv, None).await.unwrap();
    assert_eq!(again, id, "the same identity signs in the same account");
    assert_eq!(users(&srv).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_state_and_reused_state_are_refused() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let bogus = format!(
        "{}/v1/auth/oidc/callback?code=abc&state=not-a-state",
        srv.base_url
    );
    assert_eq!(back_to_app(&bogus).await, Err(400));
    let no_state = format!("{}/v1/auth/oidc/callback?code=abc", srv.base_url);
    assert_eq!(back_to_app(&no_state).await, Err(400));

    let app = App::new();
    let callback = to_provider(&srv, &app, None, None).await;
    let first = back_to_app(&callback).await.unwrap();
    assert!(first.contains_key("code"), "{first:?}");
    assert_eq!(back_to_app(&callback).await, Err(400), "a state works once");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_flow_is_refused() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let callback = to_provider(&srv, &App::new(), None, None).await;
    server::sqlx::query("UPDATE oidc_flows SET expires_at = now() - interval '1 second'")
        .execute(srv.pool())
        .await
        .unwrap();
    assert_eq!(back_to_app(&callback).await, Err(400));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_nonce_wrong_audience_and_expired_tokens_are_refused() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    type Spoil = fn(&mut Next);
    let spoilers: [(&str, Spoil); 3] = [
        ("nonce", |n| n.nonce = Some("someone-elses-nonce".into())),
        ("audience", |n| n.aud = Some("another-client".into())),
        ("expiry", |n| n.exp_offset = -3600),
    ];
    for (what, spoil) in spoilers {
        set_next(&fake, |n| {
            *n = Next::person("sub-1", "ada@example.org");
            spoil(n);
        });
        let (code, _) = sign_in(&srv, None).await.unwrap_err();
        assert_eq!(code, "token_invalid", "{what}");
    }
    assert_eq!(users(&srv).await, 0, "nothing was stored");
    assert_eq!(identities(&srv).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_intercepted_code_is_useless_without_its_pkce_verifier() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let victim = to_provider(&srv, &App::new(), None, None).await;
    let stolen = url::Url::parse(&victim).unwrap();
    let stolen_code = stolen
        .query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .into_owned();

    // Redeemed at the provider directly, without the verifier the server holds.
    let resp = reqwest::Client::new()
        .post(format!("{}/token", fake.issuer))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", stolen_code.as_str()),
            (
                "redirect_uri",
                &format!("{}/v1/auth/oidc/callback", srv.base_url),
            ),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Injected into the attacker's own flow: the server proves that flow's verifier, which
    // doesn't match the stolen code's challenge.
    let attacker = to_provider(&srv, &App::new(), None, None).await;
    let attacker_state = url::Url::parse(&attacker)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let mut injected = url::Url::parse(&format!("{}/v1/auth/oidc/callback", srv.base_url)).unwrap();
    injected
        .query_pairs_mut()
        .append_pair("code", &stolen_code)
        .append_pair("state", &attacker_state);
    let back = back_to_app(injected.as_str()).await.unwrap();
    assert_eq!(back["error"], "provider_refused", "{back:?}");
    assert_eq!(users(&srv).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_code_works_once_and_only_with_the_apps_verifier() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let app = App::new();
    let back = back_to_app(&to_provider(&srv, &app, None, None).await)
        .await
        .unwrap();
    let (s, _) = exchange(&srv, &back["code"], &app.verifier).await;
    assert_eq!(s, 200);
    let (s, v) = exchange(&srv, &back["code"], &app.verifier).await;
    assert_eq!(s, 400, "a reused handoff code is refused: {v}");

    // A redirect caught by another local program: its code needs the app's verifier.
    let app = App::new();
    let back = back_to_app(&to_provider(&srv, &app, None, None).await)
        .await
        .unwrap();
    let (s, _) = exchange(&srv, &back["code"], &server::auth::new_token()).await;
    assert_eq!(s, 400);
    let (s, _) = exchange(&srv, &back["code"], &app.verifier).await;
    assert_eq!(s, 400, "a wrong guess spends the code");

    let sessions: i64 = server::sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 1, "only the first exchange made a session");
}

#[tokio::test(flavor = "multi_thread")]
async fn return_targets_must_be_loopback() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    for target in [
        "https://evil.example/oidc",
        "http://localhost:53111/oidc",
        "commons://oidc",
    ] {
        let (s, _) = call(
            &srv,
            Method::POST,
            None,
            "/auth/oidc/start",
            Some(serde_json::json!({
                "return_to": target,
                "code_challenge": server::oidc::pkce_challenge("v"),
            })),
        )
        .await;
        assert_eq!(s, 400, "{target}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_mode_needs_a_valid_invite_and_redeems_it() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Invite).await;
    let owner = register(&srv, "owner").await;
    let w = workspace(&srv, &owner, &[]).await;

    let (code, _) = sign_in(&srv, None).await.unwrap_err();
    assert_eq!(code, "invite_required");
    let (code, _) = sign_in(&srv, Some("not-a-token")).await.unwrap_err();
    assert_eq!(code, "invite_invalid");
    assert_eq!(identities(&srv).await, 0);

    let v = ok(
        &srv,
        Method::POST,
        &owner,
        &format!("/workspaces/{}/invites", w.id),
        Some(serde_json::json!({"role": "guest", "email": "ada@example.org"})),
    )
    .await;
    let invite: InviteCreated = serde_json::from_value(v).unwrap();
    let (_, id) = sign_in(&srv, Some(&invite.link)).await.unwrap();
    let role: String = server::sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(w.id)
    .bind(id)
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert_eq!(role, "guest", "joined with the invite's role");

    let (code, _) = sign_in(&srv, Some(&invite.link)).await.unwrap_err();
    assert_eq!(
        code, "invite_used",
        "an existing identity still checks the invite"
    );
}

async fn invite_for(srv: &TestServer, owner: &Person, ws: Uuid, email: &str) -> InviteCreated {
    let v = ok(
        srv,
        Method::POST,
        owner,
        &format!("/workspaces/{ws}/invites"),
        Some(serde_json::json!({"role": "member", "email": email})),
    )
    .await;
    serde_json::from_value(v).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_account_needs_the_invited_address_verified_by_the_provider() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Invite).await;
    let owner = register(&srv, "owner").await;
    let w = workspace(&srv, &owner, &[]).await;
    let inv = invite_for(&srv, &owner, w.id, "Pat@Example.org").await;

    // A different address, then the right one unverified: refused, nothing stored.
    set_next(&fake, |n| *n = Next::person("sub-m", "mallory@example.org"));
    let (code, message) = sign_in(&srv, Some(&inv.link)).await.unwrap_err();
    assert_eq!(code, "invite_email_mismatch");
    assert!(!message.contains("pat@"), "{message}");
    set_next(&fake, |n| {
        *n = Next::person("sub-p", "pat@example.org");
        n.email_verified = false;
    });
    let (code, _) = sign_in(&srv, Some(&inv.link)).await.unwrap_err();
    assert_eq!(code, "invite_email_mismatch");
    assert_eq!((users(&srv).await, identities(&srv).await), (1, 0));

    // The invited address, verified, in another case.
    set_next(&fake, |n| *n = Next::person("sub-p", "PAT@example.org"));
    let (_, id) = sign_in(&srv, Some(&inv.link)).await.unwrap();
    let email: String = server::sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert_eq!(email, "pat@example.org");

    // An existing identity signing in with someone else's invite: refused.
    let w2 = workspace(&srv, &owner, &[]).await;
    let theirs = invite_for(&srv, &owner, w2.id, "quinn@example.org").await;
    let (code, _) = sign_in(&srv, Some(&theirs.link)).await.unwrap_err();
    assert_eq!(code, "invite_email_mismatch");
    let member: bool = server::sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND user_id = $2)",
    )
    .bind(w2.id)
    .bind(id)
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert!(!member);
}

#[tokio::test(flavor = "multi_thread")]
async fn closed_mode_refuses_new_accounts() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Closed).await;
    let (code, _) = sign_in(&srv, None).await.unwrap_err();
    assert_eq!(code, "registration_closed");
    assert_eq!(users(&srv).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_matching_email_is_never_linked_automatically() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let alice = register(&srv, "alice").await;
    set_next(&fake, |n| *n = Next::person("sub-a", &alice.email));
    let (code, message) = sign_in(&srv, None).await.unwrap_err();
    assert_eq!(code, "email_taken");
    assert!(message.contains("link Google in Settings"), "{message}");
    assert_eq!(identities(&srv).await, 0);
    assert_eq!(users(&srv).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_in_user_links_and_unlinks_explicitly() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let alice = register(&srv, "alice").await;
    set_next(&fake, |n| {
        *n = Next::person("sub-a", "alice-at-google@example.org")
    });

    // Linking needs a session.
    let (s, _) = call(
        &srv,
        Method::POST,
        None,
        "/auth/oidc/start",
        Some(serde_json::json!({
            "return_to": RETURN_TO,
            "code_challenge": server::oidc::pkce_challenge("v"),
            "link": true,
        })),
    )
    .await;
    assert_eq!(s, 401);

    let app = App::new();
    let back = back_to_app(&to_provider(&srv, &app, None, Some(&alice.token)).await)
        .await
        .unwrap();
    let (s, v) = exchange_as(&srv, &back["code"], &app.verifier, Some(&alice.token)).await;
    assert_eq!(s, 200, "{v}");
    let OidcExchanged::Linked { identities: list } = serde_json::from_value(v).unwrap() else {
        panic!("a link answers with the identities");
    };
    assert!(list.has_password);
    assert_eq!(list.identities.len(), 1);
    assert_eq!(
        list.identities[0].email.as_deref(),
        Some("alice-at-google@example.org")
    );

    // The provider now signs in Alice's account.
    let (_, id) = sign_in(&srv, None).await.unwrap();
    assert_eq!(id, alice.id);

    // The same provider account can't link to someone else.
    let bob = register(&srv, "bob").await;
    let back = back_to_app(&to_provider(&srv, &App::new(), None, Some(&bob.token)).await)
        .await
        .unwrap();
    assert_eq!(back["error"], "identity_in_use");

    // Alice has a password, so she may unlink.
    let identity = list.identities[0].id;
    let (s, _) = call(
        &srv,
        Method::DELETE,
        Some(&alice.token),
        &format!("/me/identities/{identity}"),
        None,
    )
    .await;
    assert_eq!(s, 204);
    let (s, _) = call(
        &srv,
        Method::DELETE,
        Some(&bob.token),
        &format!("/me/identities/{identity}"),
        None,
    )
    .await;
    assert_eq!(s, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_way_to_sign_in_cant_be_unlinked() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let (token, _) = sign_in(&srv, None).await.unwrap();
    let (_, v) = call(&srv, Method::GET, Some(&token), "/me/identities", None).await;
    assert_eq!(v["has_password"], false);
    let first = v["identities"][0]["id"].as_str().unwrap().to_string();
    let (s, v) = call(
        &srv,
        Method::DELETE,
        Some(&token),
        &format!("/me/identities/{first}"),
        None,
    )
    .await;
    assert_eq!(s, 403);
    assert_eq!(v["error"], "last_sign_in_method");

    // With a second identity linked, the first can go.
    set_next(&fake, |n| {
        *n = Next::person("sub-2", "ada-work@example.org")
    });
    let app = App::new();
    let back = back_to_app(&to_provider(&srv, &app, None, Some(&token)).await)
        .await
        .unwrap();
    let (s, _) = exchange_as(&srv, &back["code"], &app.verifier, Some(&token)).await;
    assert_eq!(s, 200);
    let (s, _) = call(
        &srv,
        Method::DELETE,
        Some(&token),
        &format!("/me/identities/{first}"),
        None,
    )
    .await;
    assert_eq!(s, 204);
    assert_eq!(identities(&srv).await, 1);
}

#[test]
fn oidc_is_on_only_with_all_four_settings() {
    let s = |v: &str| Some(v.to_string());
    let on = OidcConfig::from_vars(
        s("https://accounts.google.com/"),
        s("id"),
        s("secret"),
        s("Google"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(on.issuer, "https://accounts.google.com");
    assert!(
        !format!("{on:?}").contains("secret"),
        "Debug hides the client secret"
    );
    assert!(
        OidcConfig::from_vars(None, None, None, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        OidcConfig::from_vars(s("https://x"), s("id"), None, None).unwrap_err(),
        vec!["COMMONS_OIDC_CLIENT_SECRET", "COMMONS_OIDC_NAME"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deactivated_account_cant_sign_in_with_the_provider() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    sign_in(&srv, None).await.unwrap();
    // A handoff made before the deactivation can't be exchanged after it.
    let app = App::new();
    let back = back_to_app(&to_provider(&srv, &app, None, None).await)
        .await
        .unwrap();
    let args = ["deactivate".to_string(), "ada@example.org".to_string()];
    server::admin::run(srv.pool(), &srv.state.config, &args)
        .await
        .unwrap();
    let (s, v) = exchange(&srv, &back["code"], &app.verifier).await;
    assert_eq!((s, v["error"].as_str()), (403, Some("account_deactivated")));
    let (code, _) = sign_in(&srv, None).await.unwrap_err();
    assert_eq!(code, "account_deactivated");
}

/// Someone who starts a link flow on their own account and gets another person to finish it
/// at the provider never holds the handoff, so nothing is linked.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_is_made_only_by_the_exchange_with_the_session_that_started_it() {
    let fake = fake_provider().await;
    let srv = spawn(&fake, Registration::Open).await;
    let mallory = register(&srv, "mallory").await;
    let bob = register(&srv, "bob").await;
    set_next(&fake, |n| *n = Next::person("sub-v", "victim@example.org"));

    let app = App::new();
    let back = back_to_app(&to_provider(&srv, &app, None, Some(&mallory.token)).await)
        .await
        .unwrap();
    assert!(back.contains_key("code"), "{back:?}");
    assert_eq!(identities(&srv).await, 0, "the callback links nothing");

    let (s, _) = exchange(&srv, &back["code"], &app.verifier).await;
    assert_eq!(s, 401, "a link exchange needs a session");
    assert_eq!(identities(&srv).await, 0);

    let app = App::new();
    let back = back_to_app(&to_provider(&srv, &app, None, Some(&mallory.token)).await)
        .await
        .unwrap();
    let (s, _) = exchange_as(&srv, &back["code"], &app.verifier, Some(&bob.token)).await;
    assert_eq!(s, 403, "and the session that started it");
    assert_eq!(identities(&srv).await, 0);
}
