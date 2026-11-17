//! Step 2: a client that crashes mid-backfill, including just before the pending marker is
//! written, completes the backfill on restart.
mod common;

use std::sync::atomic::Ordering;

use common::*;
use proto::model::Conversation;

async fn private_channel_with_history(
    n: usize,
) -> (server::testing::TestServer, Person, Person, Conversation) {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let c: Conversation = post(
        &srv,
        Some(&alice.token),
        &format!("/workspaces/{}/channels", ws.id),
        serde_json::json!({"name": "secret", "kind": "private"}),
    )
    .await;
    for i in 0..n {
        let _: serde_json::Value = post(
            &srv,
            Some(&alice.token),
            &format!("/conversations/{}/messages", c.id),
            serde_json::json!({"client_id": uuid::Uuid::now_v7(), "body": format!("m{i}")}),
        )
        .await;
    }
    (srv, alice, bob, c)
}

async fn add(srv: &server::testing::TestServer, alice: &Person, bob: &Person, c: &Conversation) {
    let _: serde_json::Value = post(
        srv,
        Some(&alice.token),
        &format!("/conversations/{}/members", c.id),
        serde_json::json!({"user_id": bob.user.id}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_before_the_pending_marker_resumes_on_restart() {
    let (srv, alice, bob, c) = private_channel_with_history(250).await;
    let dir = tempfile::tempdir().unwrap();
    let b = account(&srv, &bob, dir.path());
    b.sync_once().await.unwrap();
    add(&srv, &alice, &bob, &c).await;

    b.faults
        .crash_before_grant_commit
        .store(true, Ordering::SeqCst);
    assert!(b.sync_once().await.is_err(), "injected crash");
    assert!(b.store.backfills().unwrap().is_empty());
    assert!(
        b.store.conversation(c.id).unwrap().is_none(),
        "nothing was written"
    );
    drop(b);

    let b = account(&srv, &bob, dir.path());
    b.sync_once().await.unwrap();
    assert_eq!(b.store.messages(c.id).unwrap().len(), 250);
    assert!(b.store.backfills().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_mid_backfill_resumes_on_restart() {
    let (srv, alice, bob, c) = private_channel_with_history(250).await;
    let dir = tempfile::tempdir().unwrap();
    let b = account(&srv, &bob, dir.path());
    b.sync_once().await.unwrap();
    add(&srv, &alice, &bob, &c).await;

    b.faults
        .crash_after_backfill_pages
        .store(1, Ordering::SeqCst);
    assert!(b.sync_once().await.is_err(), "injected crash");
    let partial = b.store.messages(c.id).unwrap().len();
    assert!(partial > 0 && partial < 250, "one page landed: {partial}");
    assert_eq!(
        b.store.backfills().unwrap().len(),
        1,
        "the marker survives the crash"
    );
    drop(b);

    let b = account(&srv, &bob, dir.path());
    b.sync_once().await.unwrap();
    let all = b.store.messages(c.id).unwrap();
    assert_eq!(all.len(), 250);
    let mut bodies: Vec<_> = all.iter().map(|m| m.body.clone()).collect();
    bodies.dedup();
    assert_eq!(bodies.len(), 250);
}
