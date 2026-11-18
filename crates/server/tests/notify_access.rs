//! Notification previews are checked against current access when delivered (decision 10):
//! replay and the live socket drop a `notify` whose message is gone or unreadable now, and
//! the cursor still advances past it.
mod common;

use std::time::Duration;

use common::*;
use reqwest::Method;
use server::testing::TestServer;
use uuid::Uuid;

async fn run_jobs(srv: &TestServer) {
    while let Some(job) = server::jobs::claim(srv.pool(), Duration::from_secs(30))
        .await
        .unwrap()
    {
        server::jobs::run_one(&srv.state, job).await;
    }
}

async fn mention(srv: &TestServer, from: &Person, conv: Uuid, to: &Person) -> Uuid {
    let body = "@bob the plan";
    let v = ok(
        srv,
        Method::POST,
        from,
        &format!("/conversations/{conv}/messages"),
        Some(
            serde_json::json!({"client_id": Uuid::now_v7(), "body": body,
            "entities": [{"kind": "mention", "user_id": to.id, "start": 0, "end": 4}]}),
        ),
    )
    .await;
    v["id"].as_str().unwrap().parse().unwrap()
}

fn notifies(events: &serde_json::Value) -> usize {
    events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["body"]["type"] == "notify")
        .count()
}

async fn user_replay(srv: &TestServer, p: &Person, since: i64) -> serde_json::Value {
    ok(
        srv,
        Method::GET,
        p,
        &format!("/sync/user?since={since}"),
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_while_offline_gets_no_preview_and_the_cursor_advances() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    let secret = channel(&srv, &alice, w.id, "secret", true).await;
    ok(
        &srv,
        Method::POST,
        &alice,
        &format!("/conversations/{}/members", secret.id),
        Some(serde_json::json!({"user_id": bob.id})),
    )
    .await;
    let since = user_seq(&srv, bob.id).await;

    mention(&srv, &alice, secret.id, &bob).await;
    run_jobs(&srv).await;
    assert_eq!(notifies(&user_replay(&srv, &bob, since).await["events"]), 1);

    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/conversations/{}/members/{}", secret.id, bob.id),
        None,
    )
    .await;
    let page = user_replay(&srv, &bob, since).await;
    assert_eq!(notifies(&page["events"]), 0, "{page}");
    assert_eq!(page["next_since"], page["head"], "the cursor advances");
    assert!(page["head"].as_i64().unwrap() > since);

    // The live socket reads the same filtered stream.
    let mut s = open_socket(&srv, &bob.token, since, &[]).await;
    let frames = s.collect(Duration::from_secs(2)).await;
    let user: Vec<_> = frames.iter().filter(|f| f["type"] == "user").collect();
    assert!(!user.is_empty());
    assert!(
        user.iter().all(|f| notifies(&f["events"]) == 0),
        "{frames:?}"
    );
    assert_eq!(user.last().unwrap()["next_since"], page["head"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_message_has_no_preview() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    let secret = channel(&srv, &alice, w.id, "secret", true).await;
    ok(
        &srv,
        Method::POST,
        &alice,
        &format!("/conversations/{}/members", secret.id),
        Some(serde_json::json!({"user_id": bob.id})),
    )
    .await;
    let since = user_seq(&srv, bob.id).await;
    let m = mention(&srv, &alice, secret.id, &bob).await;
    run_jobs(&srv).await;
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/messages/{m}"),
        None,
    )
    .await;
    let page = user_replay(&srv, &bob, since).await;
    assert_eq!(notifies(&page["events"]), 0, "{page}");
    assert_eq!(page["next_since"], page["head"]);
}
