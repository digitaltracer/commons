//! Presence goes only to workspaces the socket passed the access check for.
mod common;

use std::time::Duration;

use common::*;
use reqwest::Method;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_member_is_not_shown_as_present() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/workspaces/{}/members/{}", w.id, bob.id),
        None,
    )
    .await;
    let mut a = open_socket(
        &srv,
        &alice.token,
        user_seq(&srv, alice.id).await,
        &[(w.id, head(&srv, w.id).await)],
    )
    .await;
    a.collect(Duration::from_millis(300)).await;

    // Bob still claims the workspace in his hello.
    let mut b = open_socket(
        &srv,
        &bob.token,
        user_seq(&srv, bob.id).await,
        &[(w.id, head(&srv, w.id).await)],
    )
    .await;
    b.collect(Duration::from_millis(500)).await;
    assert_eq!(srv.state.hub.connections(bob.id), 1, "bob is connected");

    let online: Vec<Uuid> = serde_json::from_value(
        ok(
            &srv,
            Method::GET,
            &alice,
            &format!("/workspaces/{}/presence", w.id),
            None,
        )
        .await,
    )
    .unwrap();
    assert!(online.contains(&alice.id));
    assert!(!online.contains(&bob.id), "{online:?}");
    let frames = a.collect(Duration::from_millis(500)).await;
    assert!(
        !frames
            .iter()
            .any(|f| f["type"] == "presence" && f["user_id"] == bob.id.to_string()),
        "{frames:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_is_announced_after_the_check() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    let mut a = open_socket(
        &srv,
        &alice.token,
        user_seq(&srv, alice.id).await,
        &[(w.id, head(&srv, w.id).await)],
    )
    .await;
    a.collect(Duration::from_millis(300)).await;
    let _b = open_socket(
        &srv,
        &bob.token,
        user_seq(&srv, bob.id).await,
        &[(w.id, head(&srv, w.id).await)],
    )
    .await;
    let seen = a
        .wait_for(Duration::from_secs(3), |f| {
            f["type"] == "presence" && f["user_id"] == bob.id.to_string() && f["online"] == true
        })
        .await;
    assert!(seen.is_some());
    let online: Vec<Uuid> = serde_json::from_value(
        ok(
            &srv,
            Method::GET,
            &alice,
            &format!("/workspaces/{}/presence", w.id),
            None,
        )
        .await,
    )
    .unwrap();
    assert!(online.contains(&bob.id));
}
