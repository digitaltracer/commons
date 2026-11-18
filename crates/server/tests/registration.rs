//! `COMMONS_REGISTRATION`: open, closed and invite (invite mode itself is in `invites.rs`).
mod common;

use common::call;
use server::config::Registration;
use server::testing::{TestServer, spawn};

async fn try_register(srv: &TestServer, email: &str) -> (u16, serde_json::Value) {
    call(
        srv,
        reqwest::Method::POST,
        None,
        "/auth/register",
        Some(serde_json::json!({
            "email": email, "password": "correct horse battery", "display_name": "P"
        })),
    )
    .await
}

#[tokio::test]
async fn open_accepts_anyone() {
    let srv = spawn(|_| {}).await;
    assert_eq!(srv.state.config.registration, Registration::Open);
    let (s, v) = try_register(&srv, "anyone@example.org").await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn closed_refuses_everyone_with_a_message() {
    let srv = spawn(|c| c.registration = Registration::Closed).await;
    let (s, v) = try_register(&srv, "anyone@example.org").await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"], "registration_closed");
    assert_eq!(v["message"], "registration is closed on this server");
    // The refused registration left no account behind.
    let (s, _) = call(
        &srv,
        reqwest::Method::POST,
        None,
        "/auth/login",
        Some(
            serde_json::json!({"email": "anyone@example.org", "password": "correct horse battery"}),
        ),
    )
    .await;
    assert_eq!(s, 401, "no account was created");
}

#[test]
fn parse_modes() {
    assert_eq!(Registration::parse(None, None).unwrap(), Registration::Open);
    assert_eq!(
        Registration::parse(Some("open"), None).unwrap(),
        Registration::Open
    );
    assert_eq!(
        Registration::parse(Some("closed"), None).unwrap(),
        Registration::Closed
    );
    assert_eq!(
        Registration::parse(Some("invite"), Some(" ")).unwrap(),
        Registration::Invite
    );
    assert!(Registration::parse(Some("sometimes"), None).is_err());
}

#[test]
fn the_old_allowlist_fails_startup_pointing_to_invite() {
    for (mode, list) in [
        (Some("allowlist"), None),
        (Some("allowlist"), Some("a@example.org")),
        (Some("invite"), Some("a@example.org")),
    ] {
        let e = Registration::parse(mode, list).unwrap_err().to_string();
        assert!(e.contains("COMMONS_REGISTRATION=invite"), "{e}");
    }
}
