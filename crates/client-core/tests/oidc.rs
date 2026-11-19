//! Provider sign-in from the core: the loopback listener, the exchange, and what is stored.
//! The "browser" follows the redirects from the provider back to the core's listener.
mod common;

use std::sync::Arc;
use std::time::Duration;

use client_core::Core;
use client_core::oidc::OidcError;
use client_core::platform::{MemorySecrets, RecordingNotifier, SecretStore};
use client_core::transport::{HttpTransport, Transport};
use common::*;
use proto::view::ConnectionState;
use server::config::Registration;
use server::fake_oidc::{Fake, Next};
use server::testing::TestServer;

async fn setup(registration: Registration) -> (Arc<Fake>, TestServer) {
    let fake = server::fake_oidc::start().await;
    let oidc = fake.config("Google");
    let srv = server::testing::spawn(move |c| {
        c.registration = registration;
        c.oidc = Some(oidc);
    })
    .await;
    (fake, srv)
}

fn core(dir: &std::path::Path, srv: &TestServer, secrets: Arc<MemorySecrets>) -> Arc<Core> {
    let url = srv.base_url.clone();
    Core::open(
        dir,
        secrets,
        Arc::new(RecordingNotifier::default()),
        Arc::new(move |_: &str| Arc::new(HttpTransport::new(&url)) as Arc<dyn Transport>),
        Arc::new(|_| {}),
    )
    .unwrap()
}

/// Opens the URL the way the system browser would, following every redirect.
fn browser(url: &str) -> anyhow::Result<()> {
    let url = url.to_string();
    tokio::spawn(async move {
        let _ = reqwest::get(url).await;
    });
    Ok(())
}

fn nowhere(_: &str) -> anyhow::Result<()> {
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_sign_in_adds_the_account() {
    let (_fake, srv) = setup(Registration::Open).await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let core = core(dir.path(), &srv, secrets.clone());
    let meta = core.server_meta(&srv.base_url).await.unwrap();
    assert_eq!(meta.oidc.unwrap().name, "Google");

    let id = core
        .add_account_oidc(&srv.base_url, None, browser)
        .await
        .unwrap();
    let acc = core.account(id).unwrap();
    assert_eq!(acc.user.email, "ada@example.org");
    assert!(secrets.get(id).unwrap().is_some(), "the session is kept");
    eventually("online", || acc.connection() == ConnectionState::Online).await;

    let list = core.link_oidc(id, nowhere);
    // A link needs the browser; cancelling it leaves the account as it was.
    let cancel = {
        let core = core.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            core.cancel_oidc();
        })
    };
    let err = list.await.unwrap_err();
    cancel.await.unwrap();
    assert!(
        matches!(err.downcast_ref::<OidcError>(), Some(OidcError::Cancelled)),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_sign_in_stores_nothing() {
    let (_fake, srv) = setup(Registration::Closed).await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let core = core(dir.path(), &srv, secrets.clone());
    let err = core
        .add_account_oidc(&srv.base_url, None, browser)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "registration is closed on this server");
    assert!(core.accounts().is_empty());
    assert!(!dir.path().join("accounts.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pasted_invite_goes_through_to_the_new_account() {
    let (_fake, srv) = setup(Registration::Invite).await;
    let owner = register(&srv, "owner").await;
    let ws = workspace(&srv, &owner, &[]).await;
    let invite: proto::api::InviteCreated = post(
        &srv,
        Some(&owner.token),
        &format!("/workspaces/{}/invites", ws.id),
        serde_json::json!({"role": "member", "email": "Ada@Example.org"}),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let core = core(dir.path(), &srv, Arc::new(MemorySecrets::default()));
    let id = core
        .add_account_oidc(&srv.base_url, Some(&invite.link), browser)
        .await
        .unwrap();
    let acc = core.account(id).unwrap();
    eventually("the invite's workspace", || {
        acc.view()
            .map(|v| v.workspaces.iter().any(|w| w.id == ws.id))
            .unwrap_or(false)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn link_then_sign_in_again_with_the_provider() {
    let (fake, srv) = setup(Registration::Open).await;
    let alice = register(&srv, "alice").await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let core = core(dir.path(), &srv, secrets.clone());
    let id = core
        .add_account(
            &srv.base_url,
            &alice.user.email,
            "correct horse battery",
            None,
            None,
        )
        .await
        .unwrap();
    fake.set(|n| *n = Next::person("sub-alice", "alice@gmail.example"));
    let list = core.link_oidc(id, browser).await.unwrap();
    assert!(list.has_password);
    assert_eq!(list.identities.len(), 1);

    // The operator signs Alice out everywhere; the provider signs her back in.
    let acc = core.account(id).unwrap();
    eventually("online", || acc.connection() == ConnectionState::Online).await;
    server::admin::run(
        srv.pool(),
        &srv.state.config,
        &["revoke-sessions".into(), alice.user.email.clone()],
    )
    .await
    .unwrap();
    eventually("signed out", || {
        acc.connection() == ConnectionState::SignedOut
    })
    .await;
    core.sign_in_again_oidc(id, browser).await.unwrap();
    eventually("online again", || {
        acc.connection() == ConnectionState::Online
    })
    .await;

    // A different provider account doesn't sign this account in, and its session ends.
    fake.set(|n| *n = Next::person("sub-bob", "bob@gmail.example"));
    let err = core.sign_in_again_oidc(id, browser).await.unwrap_err();
    assert!(
        err.to_string().contains("belongs to bob@gmail.example"),
        "{err}"
    );
    let live: i64 = server::sqlx::query_scalar(
        "SELECT count(*) FROM sessions s JOIN users u ON u.id = s.user_id
          WHERE u.email = 'bob@gmail.example' AND s.revoked_at IS NULL",
    )
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert_eq!(live, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_that_never_comes_back_times_out() {
    let (_fake, srv) = setup(Registration::Open).await;
    let transport = HttpTransport::new(&srv.base_url);
    let never = tokio::sync::Notify::new();
    let err = client_core::oidc::run(
        &transport,
        None,
        None,
        false,
        nowhere,
        &never,
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err.downcast_ref::<OidcError>(), Some(OidcError::TimedOut)),
        "{err}"
    );
}

async fn invite_for(srv: &TestServer, owner: &Person, ws: uuid::Uuid, email: &str) -> String {
    let invite: proto::api::InviteCreated = post(
        srv,
        Some(&owner.token),
        &format!("/workspaces/{ws}/invites"),
        serde_json::json!({"role": "member", "email": email}),
    )
    .await;
    format!(
        "commons://invite?server={}#{}",
        srv.base_url.replace(':', "%3A").replace('/', "%2F"),
        invite.token
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opened_invite_link_previews_then_signs_up_with_the_provider() {
    let (_fake, srv) = setup(Registration::Invite).await;
    let owner = register(&srv, "owner").await;
    let ws = workspace(&srv, &owner, &[]).await;
    let dir = tempfile::tempdir().unwrap();
    let core = core(dir.path(), &srv, Arc::new(MemorySecrets::default()));

    let link = invite_for(&srv, &owner, ws.id, "ada@example.org").await;
    let opened = core.open_invite_link(&link).await.unwrap();
    assert_eq!(opened.server_url, srv.base_url);
    assert_eq!(opened.preview.workspace_name, ws.name);
    assert_eq!(opened.preview.inviter_name, "owner");
    assert_eq!(
        opened.preview.email_masked,
        "a\u{2022}\u{2022}\u{2022}@example.org"
    );
    assert!(opened.accounts.is_empty(), "a new server to this app");
    assert!(core.accounts().is_empty(), "opening a link adds nothing");

    let id = core
        .add_account_oidc(&opened.server_url, Some(&opened.token), browser)
        .await
        .unwrap();
    let acc = core.account(id).unwrap();
    eventually("the invite's workspace", || {
        acc.view()
            .map(|v| v.workspaces.iter().any(|w| w.id == ws.id))
            .unwrap_or(false)
    })
    .await;

    // The same server again: the account is offered, and the link is refused now it's used.
    let next = invite_for(&srv, &owner, ws.id, "bob@example.org").await;
    assert_eq!(
        core.open_invite_link(&next).await.unwrap().accounts,
        vec![id]
    );
    assert_eq!(core.accounts_on(&format!("{}/", srv.base_url)), vec![id]);
    assert!(core.accounts_on("https://elsewhere.example").is_empty());
    let err = core.open_invite_link(&link).await.unwrap_err().to_string();
    assert!(err.starts_with("This invite link isn't valid."), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opened_invite_link_signs_up_with_a_password() {
    let (_fake, srv) = setup(Registration::Invite).await;
    let owner = register(&srv, "owner").await;
    let ws = workspace(&srv, &owner, &[]).await;
    let dir = tempfile::tempdir().unwrap();
    let core = core(dir.path(), &srv, Arc::new(MemorySecrets::default()));
    let link = invite_for(&srv, &owner, ws.id, "cy@example.org").await;
    let opened = core.open_invite_link(&link).await.unwrap();
    let id = core
        .add_account(
            &opened.server_url,
            "cy@example.org",
            "correct horse battery",
            Some("Cy"),
            Some(&opened.token),
        )
        .await
        .unwrap();
    let acc = core.account(id).unwrap();
    eventually("the invite's workspace", || {
        acc.view()
            .map(|v| v.workspaces.iter().any(|w| w.id == ws.id))
            .unwrap_or(false)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_link_sends_nothing() {
    let (_fake, srv) = setup(Registration::Invite).await;
    let dir = tempfile::tempdir().unwrap();
    let core = core(dir.path(), &srv, Arc::new(MemorySecrets::default()));
    let token = format!("{}.{}", uuid::Uuid::now_v7().simple(), "A".repeat(43));
    for raw in [
        format!("commons://invite?server=http://evil.example#{token}"),
        format!("commons://reset?server=https://x.org#{token}"),
        "commons://invite?server=https://x.org#junk".to_string(),
    ] {
        let err = core.open_invite_link(&raw).await.unwrap_err();
        assert!(
            err.downcast_ref::<client_core::deeplink::LinkError>()
                .is_some(),
            "refused before any request: {err}"
        );
    }
}
