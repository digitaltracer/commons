//! Step 2 checks on the server: delivery needs both the access interval and current access.
mod common;

use common::*;
use reqwest::Method;

fn contents(replay: &serde_json::Value) -> Vec<String> {
    replay["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["body"]["message"]["body"].as_str().map(str::to_string))
        .collect()
}

fn kinds(replay: &serde_json::Value) -> Vec<String> {
    replay["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"]["type"].as_str().unwrap().to_string())
        .collect()
}

/// Join at 10, disconnect at 12, a message at 15, removal at 20, resume from 12:
/// nothing from 15 may be delivered.
#[tokio::test(flavor = "multi_thread")]
async fn reconnect_after_removal_delivers_no_content() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let scratch = channel(&srv, &alice, ws.id, "scratch", true).await;
    let secret = channel(&srv, &alice, ws.id, "secret", true).await;

    pad_to(&srv, &alice, scratch.id, ws.id, 9).await;
    ok(
        &srv,
        Method::POST,
        &alice,
        &format!("/conversations/{}/members", secret.id),
        Some(serde_json::json!({"user_id": bob.id})),
    )
    .await;
    assert_eq!(head(&srv, ws.id).await, 10, "bob joins at 10");
    pad_to(&srv, &alice, scratch.id, ws.id, 12).await;
    // Bob is caught up to 12 and goes offline.
    pad_to(&srv, &alice, scratch.id, ws.id, 14).await;
    post(&srv, &alice, secret.id, "secret at 15").await;
    assert_eq!(head(&srv, ws.id).await, 15);
    pad_to(&srv, &alice, scratch.id, ws.id, 19).await;
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/conversations/{}/members/{}", secret.id, bob.id),
        None,
    )
    .await;
    assert_eq!(head(&srv, ws.id).await, 20, "bob is removed at 20");

    let (status, page) = replay(&srv, &bob, ws.id, 12).await;
    assert_eq!(status, 200);
    assert!(
        contents(&page).is_empty(),
        "content leaked: {:?}",
        contents(&page)
    );
    assert_eq!(
        kinds(&page),
        vec!["member_removed"],
        "only his own removal: {page}"
    );
    assert_eq!(page["next_since"], 20);

    // History is gone for him too.
    let (status, _) = call(
        &srv,
        Method::GET,
        Some(&bob.token),
        &format!("/conversations/{}/messages", secret.id),
        None,
    )
    .await;
    assert_eq!(status, 403);
}

/// Access that came from a role ends with the role, including for replay.
#[tokio::test(flavor = "multi_thread")]
async fn role_downgrade_stops_replay_of_private_content() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let carol = register(&srv, "carol").await;
    let ws = workspace(&srv, &alice, &[&carol]).await;
    let secret = channel(&srv, &alice, ws.id, "secret", true).await;
    ok(
        &srv,
        Method::PATCH,
        &alice,
        &format!("/workspaces/{}/members/{}", ws.id, carol.id),
        Some(serde_json::json!({"role": "admin"})),
    )
    .await;
    let since = head(&srv, ws.id).await;
    post(&srv, &alice, secret.id, "while admin").await;
    // Carol can read it as an admin.
    let (_, page) = replay(&srv, &carol, ws.id, since).await;
    assert_eq!(contents(&page), vec!["while admin"]);

    ok(
        &srv,
        Method::PATCH,
        &alice,
        &format!("/workspaces/{}/members/{}", ws.id, carol.id),
        Some(serde_json::json!({"role": "member"})),
    )
    .await;
    let (_, page) = replay(&srv, &carol, ws.id, since).await;
    assert!(
        contents(&page).is_empty(),
        "replayed after downgrade: {page}"
    );
    assert!(kinds(&page).contains(&"member_removed".to_string()));
}

/// A user removed from the workspace cannot resume its stream, and their own stream
/// carries the removal.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_removal_is_on_the_user_stream() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let (_, before) = call(
        &srv,
        Method::GET,
        Some(&bob.token),
        "/sync/user?since=0",
        None,
    )
    .await;
    let since = before["next_since"].as_i64().unwrap();
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/workspaces/{}/members/{}", ws.id, bob.id),
        None,
    )
    .await;
    let (status, _) = replay(&srv, &bob, ws.id, 0).await;
    assert_eq!(status, 403);
    let (_, after) = call(
        &srv,
        Method::GET,
        Some(&bob.token),
        &format!("/sync/user?since={since}"),
        None,
    )
    .await;
    assert_eq!(
        after["events"][0]["body"]["type"], "workspace_removed",
        "{after}"
    );
}

/// Every replay response advances `next_since`, even when every event was filtered out.
#[tokio::test(flavor = "multi_thread")]
async fn next_since_advances_over_filtered_ranges() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let secret = channel(&srv, &alice, ws.id, "secret", true).await;
    for _ in 0..30 {
        post(&srv, &alice, secret.id, "hidden").await;
    }
    let head = head(&srv, ws.id).await;
    let mut since = 0;
    let mut pages = 0;
    while since < head {
        let (_, page) = call(
            &srv,
            Method::GET,
            Some(&bob.token),
            &format!("/sync/workspaces/{}?since={since}&limit=4", ws.id),
            None,
        )
        .await;
        let next = page["next_since"].as_i64().unwrap();
        assert!(next > since, "stuck at {since}: {page}");
        assert!(contents(&page).is_empty());
        since = next;
        pages += 1;
    }
    assert!(pages >= 8);
}

/// A cursor before the prune point gets a snapshot at a committed watermark.
#[tokio::test(flavor = "multi_thread")]
async fn pruned_cursor_gets_a_snapshot() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let secret = channel(&srv, &alice, ws.id, "secret", true).await;
    post(&srv, &alice, secret.id, "x").await;
    let h = head(&srv, ws.id).await;
    let mut tx = srv.pool().begin().await.unwrap();
    server::sync::prune_workspace_to(&mut tx, ws.id, h - 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (_, page) = replay(&srv, &bob, ws.id, 1).await;
    let snap = &page["snapshot"];
    assert_eq!(snap["seq"], h);
    assert_eq!(page["next_since"], h);
    let names: Vec<_> = snap["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(names, vec!["general"], "bob reads only general");
}
