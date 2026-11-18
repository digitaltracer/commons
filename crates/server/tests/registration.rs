//! `COMMONS_REGISTRATION`: open, closed and allowlist.
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

#[tokio::test]
async fn allowlist_admits_listed_emails_after_normalisation() {
    let rule = Registration::parse(
        Some("allowlist"),
        Some(" Ada@Example.org , grace@example.org,"),
    )
    .unwrap();
    let srv = spawn(move |c| c.registration = rule).await;

    let (s, v) = try_register(&srv, "  ADA@example.ORG ").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["user"]["email"], "ada@example.org");
    let (s, v) = try_register(&srv, "grace@example.org").await;
    assert_eq!(s, 200, "{v}");

    let (s, v) = try_register(&srv, "mallory@example.org").await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"], "registration_not_allowed");
    assert!(v["message"].as_str().unwrap().contains("registration list"));
}

#[test]
fn parse_modes() {
    assert_eq!(Registration::parse(None, None).unwrap(), Registration::Open);
    assert_eq!(
        Registration::parse(Some("open"), Some("x@y.z")).unwrap(),
        Registration::Open
    );
    assert_eq!(
        Registration::parse(Some("closed"), None).unwrap(),
        Registration::Closed
    );
    assert!(Registration::parse(Some("allowlist"), None).is_err());
    assert!(Registration::parse(Some("allowlist"), Some(" , ")).is_err());
    assert!(Registration::parse(Some("invite"), None).is_err());
}
