//! Step 3: edits with a revision check, delete wins, raw HTML stays text, notifications are
//! deduplicated and re-checked, and read cursors follow the user across devices.
mod common;

use std::sync::Arc;

use client_core::platform::RecordingNotifier;
use client_core::store::OutboxState;
use common::*;
use proto::markdown::{Node, render};
use proto::model::{Message, NotifyReason};
use proto::view::DraftReason;
use reqwest::Method;
use server::sqlx;
use uuid::Uuid;

async fn setup() -> (server::testing::TestServer, Person, Person, Uuid, Uuid) {
    // No background worker: the tests run notify jobs themselves, at known points.
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let general = {
        let (_, v) = request(
            &srv,
            Method::GET,
            &alice.token,
            &format!("/workspaces/{}/conversations", ws.id),
            None,
        )
        .await;
        v["conversations"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    (srv, alice, bob, ws.id, general)
}

async fn send(srv: &server::testing::TestServer, p: &Person, conv: Uuid, body: &str) -> Message {
    let (s, v) = request(
        srv,
        Method::POST,
        &p.token,
        &format!("/conversations/{conv}/messages"),
        Some(serde_json::json!({"client_id": Uuid::now_v7(), "body": body})),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    serde_json::from_value(v).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_edit_comes_back_as_a_draft() {
    let (srv, alice, _bob, _ws, general) = setup().await;
    let m = send(&srv, &alice, general, "original").await;
    let laptop = account_with_store(&srv, &alice, memory());
    let phone = account_with_store(&srv, &alice, memory());
    laptop.sync_once().await.unwrap();
    phone.sync_once().await.unwrap();

    laptop
        .edit_message(general, m.id, 0, "from the laptop".into())
        .unwrap();
    phone
        .edit_message(general, m.id, 0, "from the phone".into())
        .unwrap();
    laptop.process_outbox().await.unwrap();
    phone.process_outbox().await.unwrap();

    let body: String = sqlx::query_scalar("SELECT body FROM messages WHERE id = $1")
        .bind(m.id)
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert_eq!(body, "from the laptop", "the stale edit was not applied");
    let drafts = phone.drafts().unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].reason, DraftReason::StaleEdit);
    assert_eq!(drafts[0].body, "from the phone");
    let cached = phone.store.message(m.id).unwrap().unwrap();
    assert_eq!(
        (cached.body.as_str(), cached.revision),
        ("from the laptop", 1)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_after_delete_is_refused_and_kept_as_a_draft() {
    let (srv, alice, _bob, _ws, general) = setup().await;
    let m = send(&srv, &alice, general, "doomed").await;
    let a = account_with_store(&srv, &alice, memory());
    a.sync_once().await.unwrap();
    let (s, _) = request(
        &srv,
        Method::DELETE,
        &alice.token,
        &format!("/messages/{}", m.id),
        None,
    )
    .await;
    assert_eq!(s, 200);
    a.edit_message(general, m.id, 0, "too late".into()).unwrap();
    a.process_outbox().await.unwrap();
    let drafts = a.drafts().unwrap();
    assert_eq!(drafts[0].reason, DraftReason::Deleted);
    assert_eq!(drafts[0].body, "too late");
    a.sync_once().await.unwrap();
    assert!(a.store.message(m.id).unwrap().unwrap().deleted);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_wins_over_concurrent_edits() {
    let (srv, alice, _bob, _ws, general) = setup().await;
    for round in 0..10 {
        let m = send(&srv, &alice, general, "race").await;
        let path = format!("/messages/{}", m.id);
        let edit = request(
            &srv,
            Method::PATCH,
            &alice.token,
            &path,
            Some(serde_json::json!({"body": format!("edit {round}"), "base_revision": 0})),
        );
        let del = request(&srv, Method::DELETE, &alice.token, &path, None);
        let ((es, _), (ds, _)) = tokio::join!(edit, del);
        assert_eq!(ds, 200);
        assert!(es == 200 || es == 410, "edit got {es}");
        let (deleted, body): (bool, String) =
            sqlx::query_as("SELECT deleted, body FROM messages WHERE id = $1")
                .bind(m.id)
                .fetch_one(srv.pool())
                .await
                .unwrap();
        assert!(deleted && body.is_empty(), "round {round}: delete must win");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn retried_edit_is_not_mistaken_for_a_stale_one() {
    let (srv, alice, _bob, _ws, general) = setup().await;
    let m = send(&srv, &alice, general, "v0").await;
    let key = Uuid::now_v7().to_string();
    let mut results = vec![];
    for _ in 0..2 {
        let resp = reqwest::Client::new()
            .patch(format!("{}/v1/messages/{}", srv.base_url, m.id))
            .bearer_auth(&alice.token)
            .header("Idempotency-Key", &key)
            .json(&serde_json::json!({"body": "v1", "base_revision": 0}))
            .send()
            .await
            .unwrap();
        results.push(resp.status().as_u16());
    }
    assert_eq!(results, vec![200, 200]);
    let rev: i64 = sqlx::query_scalar("SELECT revision FROM messages WHERE id = $1")
        .bind(m.id)
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert_eq!(rev, 1, "applied once");
}

#[tokio::test(flavor = "multi_thread")]
async fn script_tag_in_a_body_renders_as_text() {
    let (srv, alice, bob, _ws, general) = setup().await;
    send(
        &srv,
        &alice,
        general,
        "look <script>alert('x')</script> **here**",
    )
    .await;
    let b = account_with_store(&srv, &bob, memory());
    b.sync_once().await.unwrap();
    let body = b.store.messages(general).unwrap()[0].body.clone();
    fn has_script_text(nodes: &[Node]) -> bool {
        nodes.iter().any(|n| match n {
            Node::Text(t) => t.contains("<script>"),
            Node::Paragraph(c) | Node::Strong(c) | Node::Emphasis(c) => has_script_text(c),
            _ => false,
        })
    }
    let nodes = render(&body);
    assert!(
        has_script_text(&nodes),
        "the tag must arrive as a text node: {nodes:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mentions_notify_once_and_respect_focus() {
    let (srv, alice, bob, _ws, general) = setup().await;
    let b_notifier = Arc::new(RecordingNotifier::default());
    let b = account_full(&srv, &bob, memory(), b_notifier.clone());
    b.sync_once().await.unwrap();
    let a = account_with_store(&srv, &alice, memory());
    a.sync_once().await.unwrap();

    a.send_message(general, "ping @bob please".into(), None, vec![])
        .unwrap();
    a.process_outbox().await.unwrap();
    run_jobs(&srv).await;
    // Running the job again must not notify twice.
    let id: Uuid = sqlx::query_scalar("SELECT id FROM messages WHERE body LIKE 'ping%'")
        .fetch_one(srv.pool())
        .await
        .unwrap();
    server::jobs::enqueue(
        &mut srv.pool().acquire().await.unwrap(),
        server::notify::JOB,
        serde_json::json!({"message_id": id}),
    )
    .await
    .unwrap();
    run_jobs(&srv).await;
    let notifies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_events WHERE kind = 'notify' AND user_id = $1",
    )
    .bind(bob.user.id)
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert_eq!(notifies, 1);

    b.sync_once().await.unwrap();
    let shown = b_notifier.0.lock().unwrap().clone();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].reason, NotifyReason::Mention);
    assert_eq!(shown[0].preview, "ping @bob please");

    // While bob is looking at #general, the core does not show the notification.
    b.set_focus(Some(general));
    a.send_message(general, "again @bob".into(), None, vec![])
        .unwrap();
    a.process_outbox().await.unwrap();
    run_jobs(&srv).await;
    b.sync_once().await.unwrap();
    assert_eq!(
        b_notifier.0.lock().unwrap().len(),
        1,
        "focused conversation is suppressed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_notification_after_access_is_revoked() {
    let (srv, alice, bob, ws, _general) = setup().await;
    let (_, c) = request(
        &srv,
        Method::POST,
        &alice.token,
        &format!("/workspaces/{ws}/channels"),
        Some(serde_json::json!({"name": "secret", "kind": "private"})),
    )
    .await;
    let secret: Uuid = c["id"].as_str().unwrap().parse().unwrap();
    request(
        &srv,
        Method::POST,
        &alice.token,
        &format!("/conversations/{secret}/members"),
        Some(serde_json::json!({"user_id": bob.user.id})),
    )
    .await;
    let b = account_with_store(&srv, &bob, memory());
    b.sync_once().await.unwrap();
    let a = account_with_store(&srv, &alice, memory());
    a.sync_once().await.unwrap();
    a.send_message(secret, "@bob secret plan".into(), None, vec![])
        .unwrap();
    a.process_outbox().await.unwrap();
    // Bob is removed before the notify job runs.
    request(
        &srv,
        Method::DELETE,
        &alice.token,
        &format!("/conversations/{secret}/members/{}", bob.user.id),
        None,
    )
    .await;
    run_jobs(&srv).await;
    let notifies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_events WHERE kind = 'notify' AND user_id = $1",
    )
    .bind(bob.user.id)
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert_eq!(notifies, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn reactions_are_idempotent_and_read_cursors_follow_the_user() {
    let (srv, alice, bob, _ws, general) = setup().await;
    let m = send(&srv, &alice, general, "react to me").await;
    for _ in 0..2 {
        let (s, _) = request(
            &srv,
            Method::PUT,
            &bob.token,
            &format!("/messages/{}/reactions/%F0%9F%91%8D", m.id),
            None,
        )
        .await;
        assert_eq!(s, 204);
    }
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE kind = 'reaction_added'")
            .fetch_one(srv.pool())
            .await
            .unwrap();
    assert_eq!(events, 1);

    let laptop = account_with_store(&srv, &bob, memory());
    let phone = account_with_store(&srv, &bob, memory());
    laptop.sync_once().await.unwrap();
    phone.sync_once().await.unwrap();
    assert_eq!(phone.store.unread(general, bob.user.id).unwrap().0, 1);
    let cached = phone.store.message(m.id).unwrap().unwrap();
    assert_eq!(cached.reactions[0].emoji, "👍");

    laptop.mark_read(general).await.unwrap();
    phone.sync_once().await.unwrap();
    assert_eq!(
        phone.store.unread(general, bob.user.id).unwrap().0,
        0,
        "read on the laptop"
    );

    // Toggling through the outbox removes bob's reaction.
    phone.toggle_reaction(general, m.id, "👍".into()).unwrap();
    phone.process_outbox().await.unwrap();
    assert!(
        phone
            .store
            .outbox()
            .unwrap()
            .iter()
            .all(|o| o.state != OutboxState::Failed)
    );
    phone.sync_once().await.unwrap();
    assert!(
        phone
            .store
            .message(m.id)
            .unwrap()
            .unwrap()
            .reactions
            .is_empty()
    );
}
