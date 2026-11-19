//! Presence goes only to workspaces the socket passed the access check for, and a socket
//! learns who is online from a list sent once it receives presence frames, so a user who
//! connects at the same moment is never missed.
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

/// Whether `user` is online after applying these frames in order.
fn sees_online(frames: &[serde_json::Value], user: Uuid) -> bool {
    let id = serde_json::json!(user.to_string());
    let mut on = false;
    for f in frames {
        match f["type"].as_str() {
            Some("online") if f["user_ids"].as_array().is_some_and(|a| a.contains(&id)) => {
                on = true
            }
            Some("presence") if f["user_id"] == id => on = f["online"] == true,
            _ => {}
        }
    }
    on
}

#[tokio::test(flavor = "multi_thread")]
async fn sockets_that_connect_together_see_each_other_online() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    let h = head(&srv, w.id).await;
    let (sa, sb) = (user_seq(&srv, alice.id).await, user_seq(&srv, bob.id).await);
    let cursors = [(w.id, h)];
    let (mut a, mut b) = tokio::join!(
        open_socket(&srv, &alice.token, sa, &cursors),
        open_socket(&srv, &bob.token, sb, &cursors),
    );
    let (fa, fb) = tokio::join!(
        a.collect(Duration::from_millis(800)),
        b.collect(Duration::from_millis(800))
    );
    assert!(sees_online(&fa, bob.id), "alice's frames: {fa:?}");
    assert!(sees_online(&fb, alice.id), "bob's frames: {fb:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_socket_gets_the_online_list() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    let h = head(&srv, w.id).await;
    let mut a = open_socket(
        &srv,
        &alice.token,
        user_seq(&srv, alice.id).await,
        &[(w.id, h)],
    )
    .await;
    a.collect(Duration::from_millis(300)).await;
    let mut b = open_socket(&srv, &bob.token, user_seq(&srv, bob.id).await, &[(w.id, h)]).await;
    let fb = b.collect(Duration::from_millis(500)).await;
    let list = fb
        .iter()
        .find(|f| f["type"] == "online" && f["workspace_id"] == w.id.to_string())
        .unwrap_or_else(|| panic!("no online list: {fb:?}"));
    let ids = list["user_ids"].as_array().unwrap();
    assert!(
        ids.contains(&serde_json::json!(alice.id.to_string())),
        "{list}"
    );
}
