//! Step 1: sign in, send, receive live over the windowed subscription, and keep history
//! across a restart.
mod common;

use std::sync::{Arc, Mutex};

use client_core::store::{SqliteStore, Storage};
use common::*;
use proto::window::{Applied, WindowBatch, WindowClient, WindowSpec};
use server::sqlx;

#[tokio::test(flavor = "multi_thread")]
async fn send_receive_live_and_survive_restart() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = account(&srv, &alice, dir_a.path());
    let b = account(&srv, &bob, dir_b.path());
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    a.start();
    b.start();
    let general_id = general(&b, ws.id);

    // Bob watches #general through a window.
    let batches: Arc<Mutex<Vec<WindowBatch>>> = Arc::default();
    let sink = batches.clone();
    let sub = b.subscribe_window(
        WindowSpec {
            account_id: b.id,
            conversation_id: general_id,
            thread_root_id: None,
            offset_from_end: 0,
            limit: 50,
        },
        Box::new(move |batch| {
            sink.lock().unwrap().push(batch);
            true
        }),
    );

    let client_id = a
        .send_message(general_id, "hello **bob**".into(), None, vec![])
        .unwrap();
    eventually("bob receives the message live", || {
        b.store
            .messages(general_id)
            .unwrap()
            .iter()
            .any(|m| m.client_id == client_id)
    })
    .await;

    let mut ui = WindowClient::new();
    eventually("the window delivers the row", || {
        let pending: Vec<_> = batches.lock().unwrap().drain(..).collect();
        for batch in pending {
            if let Applied::Ack(rev) = ui.apply(batch) {
                b.windows.ack(sub, ui.generation, rev, &*b.0);
            }
        }
        ui.rows.iter().any(|r| r.key == client_id)
    })
    .await;
    assert_eq!(
        ui.rows
            .iter()
            .find(|r| r.key == client_id)
            .unwrap()
            .message
            .body,
        "hello **bob**"
    );

    // Restart: drop the account, reopen the same cache with the server gone.
    b.stop();
    a.stop();
    drop(b);
    drop(srv);
    let store = SqliteStore::open(&dir_b.path().join("cache.sqlite")).unwrap();
    let history = store.messages(general_id).unwrap();
    assert!(
        history
            .iter()
            .any(|m| m.client_id == client_id && m.body == "hello **bob**")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resend_with_same_client_id_is_a_no_op() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let dir = tempfile::tempdir().unwrap();
    let a = account(&srv, &alice, dir.path());
    a.sync_once().await.unwrap();
    let general_id = general(&a, ws.id);
    let client_id = uuid::Uuid::now_v7();
    let body = serde_json::json!({"client_id": client_id, "body": "once"});
    let path = format!("/conversations/{general_id}/messages");
    let (s1, m1) = request(
        &srv,
        reqwest::Method::POST,
        &alice.token,
        &path,
        Some(body.clone()),
    )
    .await;
    let (s2, m2) = request(&srv, reqwest::Method::POST, &alice.token, &path, Some(body)).await;
    assert_eq!((s1, s2), (200, 200));
    assert_eq!(m1["id"], m2["id"]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE client_id = $1")
        .bind(client_id)
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE kind = 'message_posted'")
            .fetch_one(srv.pool())
            .await
            .unwrap();
    assert_eq!(events, 1);
}
