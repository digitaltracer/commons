//! Guests can't browse public channels, so they never hear that one was created.
mod common;

use std::time::Duration;

use common::*;
use reqwest::Method;

fn created(events: &serde_json::Value) -> Vec<String> {
    events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["body"]["type"] == "conversation_created")
        .map(|e| {
            e["body"]["conversation"]["name"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn guests_get_no_conversation_created() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let gus = register(&srv, "gus").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    ok(
        &srv,
        Method::POST,
        &alice,
        &format!("/workspaces/{}/members", w.id),
        Some(serde_json::json!({"email": gus.email, "role": "guest"})),
    )
    .await;
    let since = head(&srv, w.id).await;
    let mut g = open_socket(
        &srv,
        &gus.token,
        user_seq(&srv, gus.id).await,
        &[(w.id, since)],
    )
    .await;
    let mut b = open_socket(
        &srv,
        &bob.token,
        user_seq(&srv, bob.id).await,
        &[(w.id, since)],
    )
    .await;
    g.collect(Duration::from_millis(300)).await;
    b.collect(Duration::from_millis(300)).await;

    channel(&srv, &alice, w.id, "launch-plans", false).await;

    let (_, page) = replay(&srv, &gus, w.id, since).await;
    assert!(created(&page["events"]).is_empty(), "guest replay: {page}");
    let (_, page) = replay(&srv, &bob, w.id, since).await;
    assert_eq!(created(&page["events"]), vec!["launch-plans"]);

    let on_socket = |frames: Vec<serde_json::Value>| -> Vec<String> {
        frames
            .iter()
            .filter(|f| f["type"] == "workspace")
            .flat_map(|f| created(&f["events"]))
            .collect()
    };
    assert!(on_socket(g.collect(Duration::from_secs(1)).await).is_empty());
    assert_eq!(
        on_socket(b.collect(Duration::from_millis(500)).await),
        vec!["launch-plans"]
    );
}
