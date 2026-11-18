//! Typing is relayed from in-memory access state: no database work per frame, a per-connection
//! limit on the sender, and nothing for someone who cannot read the conversation.
mod common;

use std::time::Duration;

use common::*;
use reqwest::Method;
use uuid::Uuid;

fn typing(conv: Uuid) -> String {
    serde_json::json!({"type": "typing", "conversation_id": conv}).to_string()
}

fn typing_frames(frames: &[serde_json::Value], conv: Uuid) -> usize {
    frames
        .iter()
        .filter(|f| f["type"] == "typing" && f["conversation_id"] == conv.to_string())
        .count()
}

async fn connect(srv: &server::testing::TestServer, p: &Person, ws: Uuid) -> Socket {
    let mut s = open_socket(
        srv,
        &p.token,
        user_seq(srv, p.id).await,
        &[(ws, head(srv, ws).await)],
    )
    .await;
    s.collect(Duration::from_millis(300)).await;
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_is_rate_limited_and_reaches_only_readers() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let carol = register(&srv, "carol").await;
    let w = workspace(&srv, &alice, &[&bob, &carol]).await;
    let secret = channel(&srv, &alice, w.id, "secret", true).await;
    ok(
        &srv,
        Method::POST,
        &alice,
        &format!("/conversations/{}/members", secret.id),
        Some(serde_json::json!({"user_id": bob.id})),
    )
    .await;
    let a = connect(&srv, &alice, w.id).await;
    let mut b = connect(&srv, &bob, w.id).await;
    let mut c = connect(&srv, &carol, w.id).await;

    for _ in 0..5 {
        a.tx.send(typing(secret.id)).unwrap();
    }
    let got = b.collect(Duration::from_millis(1200)).await;
    assert_eq!(
        typing_frames(&got, secret.id),
        1,
        "one frame per 2 s: {got:?}"
    );
    assert_eq!(
        typing_frames(&c.collect(Duration::from_millis(200)).await, secret.id),
        0,
        "a non-reader gets nothing"
    );

    tokio::time::sleep(Duration::from_millis(1000)).await;
    a.tx.send(typing(secret.id)).unwrap();
    let got = b.collect(Duration::from_millis(800)).await;
    assert_eq!(
        typing_frames(&got, secret.id),
        1,
        "the limit lets the next one through"
    );

    // A conversation the sender cannot read relays nothing.
    let d = connect(&srv, &carol, w.id).await;
    d.tx.send(typing(secret.id)).unwrap();
    assert_eq!(
        typing_frames(&b.collect(Duration::from_millis(800)).await, secret.id),
        0
    );
}
