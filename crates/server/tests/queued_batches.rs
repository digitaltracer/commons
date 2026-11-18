//! A feed batch already queued ahead of a removal is judged by access after the removal
//! (decision 10: queued socket events are checked again). The test runtime is
//! single-threaded, so batches handed to the hub back to back are both queued before the
//! socket task runs.
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use reqwest::Method;
use server::feed::{Batch, RawEvent};
use server::hub::Ephemeral;
use uuid::Uuid;

fn message(seq: i64, conv: Uuid) -> RawEvent {
    RawEvent {
        seq,
        audience: 1,
        conversation_id: Some(conv),
        subject: None,
        membership: None,
        left: None,
        joined: None,
        guest: None,
        json: format!(r#"{{"seq":{seq},"body":{{"type":"message_posted","marker":"queued"}}}}"#),
    }
}

fn removal(seq: i64, conv: Uuid, user: Uuid) -> RawEvent {
    RawEvent {
        seq,
        audience: 1,
        conversation_id: Some(conv),
        subject: Some(user),
        membership: Some((conv, user, false, false)),
        left: None,
        joined: None,
        guest: None,
        json: format!(r#"{{"seq":{seq},"body":{{"type":"member_removed","marker":"removal"}}}}"#),
    }
}

fn batch(ws: Uuid, from: i64, events: Vec<RawEvent>) -> Arc<Batch> {
    let to = from + events.len() as i64;
    Arc::new(Batch {
        workspace_id: ws,
        from,
        to,
        head: to,
        events,
    })
}

fn markers(frames: &[serde_json::Value]) -> Vec<String> {
    frames
        .iter()
        .flat_map(|f| match f["type"].as_str() {
            Some("workspace") => f["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["body"]["marker"].as_str().unwrap_or("").to_string())
                .collect(),
            Some(t) => vec![t.to_string()],
            None => vec![],
        })
        .collect()
}

async fn setup() -> (server::testing::TestServer, Person, Uuid, Uuid, Socket, i64) {
    let srv = server::testing::spawn(|c| c.repair_interval = Duration::from_secs(600)).await;
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
    let h = head(&srv, w.id).await;
    let mut s = open_socket(&srv, &bob.token, user_seq(&srv, bob.id).await, &[(w.id, h)]).await;
    // Let the socket catch up and load its access state.
    s.collect(Duration::from_millis(500)).await;
    (srv, bob, w.id, secret.id, s, h)
}

#[tokio::test]
async fn a_batch_queued_ahead_of_a_removal_is_not_delivered() {
    let (srv, bob, ws, secret, mut s, h) = setup().await;
    srv.state
        .hub
        .batch(batch(ws, h, vec![message(h + 1, secret)]));
    srv.state
        .hub
        .batch(batch(ws, h + 1, vec![removal(h + 2, secret, bob.id)]));
    srv.state.hub.ephemeral(Ephemeral {
        workspace_id: ws,
        frame: proto::events::ServerFrame::Presence {
            user_id: Uuid::now_v7(),
            online: true,
        },
    });
    let frames = s.collect(Duration::from_secs(1)).await;
    assert_eq!(
        markers(&frames),
        vec!["removal", "presence"],
        "the message is withheld, the removal is delivered, and the later wakeup follows"
    );
    assert_eq!(frames[0]["next_since"], h + 2);
}

#[tokio::test]
async fn a_batch_with_nothing_queued_behind_it_is_delivered() {
    let (srv, _bob, ws, secret, mut s, h) = setup().await;
    srv.state
        .hub
        .batch(batch(ws, h, vec![message(h + 1, secret)]));
    let frames = s.collect(Duration::from_secs(1)).await;
    assert_eq!(markers(&frames), vec!["queued"]);
}

#[tokio::test]
async fn a_removal_queued_behind_typing_still_withholds_the_batch() {
    let (srv, bob, ws, secret, mut s, h) = setup().await;
    let general: Uuid = server::sqlx::query_scalar(
        "SELECT id FROM conversations WHERE workspace_id = $1 AND name = 'general'",
    )
    .bind(ws)
    .fetch_one(srv.pool())
    .await
    .unwrap();
    srv.state
        .hub
        .batch(batch(ws, h, vec![message(h + 1, secret)]));
    srv.state.hub.ephemeral(Ephemeral {
        workspace_id: ws,
        frame: proto::events::ServerFrame::Typing {
            workspace_id: ws,
            conversation_id: general,
            user_id: Uuid::now_v7(),
        },
    });
    srv.state
        .hub
        .batch(batch(ws, h + 1, vec![removal(h + 2, secret, bob.id)]));
    let frames = s.collect(Duration::from_secs(1)).await;
    assert_eq!(
        markers(&frames),
        vec!["removal", "typing"],
        "the removal behind the typing wakeup withholds the message; typing follows"
    );
}
