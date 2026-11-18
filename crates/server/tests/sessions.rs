//! Revoking a session closes the sockets opened with it, on every node, and only those.
mod common;

use std::time::Duration;

use common::*;
use reqwest::Method;

#[tokio::test(flavor = "multi_thread")]
async fn logout_closes_that_sessions_socket_only() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let w = workspace(&srv, &alice, &[]).await;
    let laptop = alice.token.clone();
    let phone = login(&srv, &alice).await;
    let general = ok(
        &srv,
        Method::GET,
        &alice,
        &format!("/workspaces/{}/conversations", w.id),
        None,
    )
    .await["conversations"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<uuid::Uuid>()
        .unwrap();
    let since = head(&srv, w.id).await;
    let us = user_seq(&srv, alice.id).await;
    let mut a = open_socket(&srv, &laptop, us, &[(w.id, since)]).await;
    let mut b = open_socket(&srv, &phone, us, &[(w.id, since)]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(srv.state.hub.connections(alice.id), 2);

    let (s, _) = call(&srv, Method::POST, Some(&laptop), "/auth/logout", None).await;
    assert_eq!(s, 204);
    assert!(
        a.closed_within(Duration::from_secs(5)).await,
        "revoked socket closes"
    );
    let (s, _) = call(&srv, Method::GET, Some(&laptop), "/me", None).await;
    assert_eq!(s, 401, "the follow-up request is refused");

    let phone_user = Person {
        token: phone.clone(),
        id: alice.id,
        email: alice.email.clone(),
    };
    post(&srv, &phone_user, general, "still here").await;
    let got = b
        .wait_for(Duration::from_secs(5), |f| {
            f["type"] == "workspace"
                && f["events"].as_array().is_some_and(|e| {
                    e.iter()
                        .any(|e| e["body"]["message"]["body"] == "still here")
                })
        })
        .await;
    assert!(got.is_some(), "the other session's socket stays open");
    assert_eq!(srv.state.hub.connections(alice.id), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn password_reset_closes_every_socket() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let w = workspace(&srv, &alice, &[]).await;
    let us = user_seq(&srv, alice.id).await;
    let mut a = open_socket(&srv, &alice.token, us, &[(w.id, head(&srv, w.id).await)]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (s, _) = call(
        &srv,
        Method::POST,
        None,
        "/auth/reset",
        Some(serde_json::json!({"email": alice.email})),
    )
    .await;
    assert_eq!(s, 202);
    let body = srv.mailer.sent.lock().unwrap().last().unwrap().2.clone();
    let token = body.split("token=").nth(1).unwrap().trim().to_string();
    let (s, v) = call(
        &srv,
        Method::POST,
        None,
        "/auth/reset/confirm",
        Some(serde_json::json!({"token": token, "password": "a new long password"})),
    )
    .await;
    assert_eq!(s, 204, "{v}");
    assert!(a.closed_within(Duration::from_secs(5)).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revocation_missed_by_the_listener_still_closes_the_socket() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let w = workspace(&srv, &alice, &[]).await;
    let other = login(&srv, &alice).await;
    let us = user_seq(&srv, alice.id).await;
    let h = head(&srv, w.id).await;
    let mut revoked = open_socket(&srv, &alice.token, us, &[(w.id, h)]).await;
    let mut kept = open_socket(&srv, &other, us, &[(w.id, h)]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Revoke with no NOTIFY, as if it was sent while the listener was down.
    server::sqlx::query("UPDATE sessions SET revoked_at = now() WHERE token_hash = $1")
        .bind(&server::auth::hash_token(&alice.token)[..])
        .execute(srv.pool())
        .await
        .unwrap();
    assert!(!revoked.closed_within(Duration::from_millis(500)).await);

    // Drop the listener's connection; it reconnects and rechecks sessions.
    let killed: i64 = server::sqlx::query_scalar(
        "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity
          WHERE datname = current_database() AND query ILIKE 'LISTEN%'",
    )
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert_eq!(killed, 1, "the listener connection");
    assert!(
        revoked.closed_within(Duration::from_secs(5)).await,
        "closed after the gap"
    );
    assert!(
        !kept.closed_within(Duration::from_millis(500)).await,
        "the live session stays"
    );
}
