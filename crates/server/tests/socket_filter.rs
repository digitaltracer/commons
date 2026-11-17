//! The socket's in-memory filter (one feed per node) must deliver exactly what the SQL replay
//! returns for the same cursor and state. Ops run one at a time; after each, every socket
//! catches up and its new events are compared with HTTPS replay from its previous cursor.
mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::*;
use futures::{SinkExt, StreamExt};
use rand::{Rng, SeedableRng};
use reqwest::Method;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

struct Sock {
    rx: mpsc::UnboundedReceiver<serde_json::Value>,
    cursor: i64,
}

async fn connect(srv: &server::testing::TestServer, p: &Person, ws: Uuid, since: i64) -> Sock {
    let mut req = srv.ws_url.clone().into_client_request().unwrap();
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", p.token).parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let user_since: i64 = server::sqlx::query_scalar("SELECT seq FROM users WHERE id = $1")
        .bind(p.id)
        .fetch_one(srv.pool())
        .await
        .unwrap();
    let hello = serde_json::json!({"type": "hello", "user_since": user_since,
        "workspaces": [{"workspace_id": ws, "since": since}]});
    socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(t))) = socket.next().await {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if tx.send(v).is_err() {
                return;
            }
        }
    });
    Sock { rx, cursor: since }
}

/// Collects workspace frames until the socket reaches `head` (or goes quiet).
async fn drain(s: &mut Sock, ws: Uuid, head: i64) -> Vec<i64> {
    let mut seqs = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while s.cursor < head {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(f)) = tokio::time::timeout(left, s.rx.recv()).await else {
            break;
        };
        if f["type"] == "workspace" && f["workspace_id"] == ws.to_string() {
            for e in f["events"].as_array().unwrap() {
                seqs.push(e["seq"].as_i64().unwrap());
            }
            s.cursor = f["next_since"].as_i64().unwrap();
        }
    }
    // Anything further for this range would also be a mismatch.
    while let Ok(f) = s.rx.try_recv() {
        if f["type"] == "workspace" && f["workspace_id"] == ws.to_string() {
            for e in f["events"].as_array().unwrap() {
                seqs.push(e["seq"].as_i64().unwrap());
            }
            s.cursor = f["next_since"].as_i64().unwrap();
        }
    }
    seqs
}

async fn replay_seqs(
    srv: &server::testing::TestServer,
    p: &Person,
    ws: Uuid,
    mut since: i64,
) -> Option<Vec<i64>> {
    let mut seqs = vec![];
    loop {
        let (status, page) = replay(srv, p, ws, since).await;
        if status == 403 {
            return None;
        }
        for e in page["events"].as_array().unwrap() {
            seqs.push(e["seq"].as_i64().unwrap());
        }
        let next = page["next_since"].as_i64().unwrap();
        if next >= page["head"].as_i64().unwrap() {
            return Some(seqs);
        }
        since = next;
    }
}

async fn run(seed: u64) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let people = vec![
        register(&srv, "u0").await,
        register(&srv, "u1").await,
        register(&srv, "u2").await,
    ];
    let refs: Vec<&Person> = people.iter().collect();
    let w = workspace(&srv, &alice, &refs).await;
    let mut convs: Vec<Uuid> = vec![];
    let v = ok(
        &srv,
        Method::GET,
        &alice,
        &format!("/workspaces/{}/conversations", w.id),
        None,
    )
    .await;
    convs.push(
        v["conversations"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );
    convs.push(channel(&srv, &alice, w.id, "s1", true).await.id);
    convs.push(channel(&srv, &alice, w.id, "s2", true).await.id);
    let start = head(&srv, w.id).await;
    let mut socks: Vec<Sock> = vec![];
    for p in &people {
        socks.push(connect(&srv, p, w.id, start).await);
    }
    let mut removed: HashMap<usize, bool> = HashMap::new();
    for step in 0..60 {
        let who = rng.gen_range(0..3);
        let conv = convs[rng.gen_range(0..3)];
        let user = people[who].id;
        let r: u32 = rng.gen_range(0..100);
        match r {
            0..=39 => {
                let author = if rng.gen_bool(0.5) {
                    &alice
                } else {
                    &people[who]
                };
                let _ = call(&srv, Method::POST, Some(&author.token), &format!("/conversations/{conv}/messages"),
                    Some(serde_json::json!({"client_id": Uuid::now_v7(), "body": format!("s{step}")}))).await;
            }
            40..=54 => {
                let _ = call(
                    &srv,
                    Method::POST,
                    Some(&alice.token),
                    &format!("/conversations/{conv}/members"),
                    Some(serde_json::json!({"user_id": user})),
                )
                .await;
            }
            55..=69 => {
                let _ = call(
                    &srv,
                    Method::DELETE,
                    Some(&alice.token),
                    &format!("/conversations/{conv}/members/{user}"),
                    None,
                )
                .await;
            }
            70..=79 => {
                let role = if rng.gen_bool(0.5) { "admin" } else { "member" };
                let _ = call(
                    &srv,
                    Method::PATCH,
                    Some(&alice.token),
                    &format!("/workspaces/{}/members/{user}", w.id),
                    Some(serde_json::json!({"role": role})),
                )
                .await;
            }
            80..=89 => {
                // Removal drops the workspace from the socket; reconnect it after re-adding.
                let _ = call(
                    &srv,
                    Method::DELETE,
                    Some(&alice.token),
                    &format!("/workspaces/{}/members/{user}", w.id),
                    None,
                )
                .await;
                removed.insert(who, true);
            }
            _ => {
                if removed.remove(&who).is_some() {
                    let _ = call(
                        &srv,
                        Method::POST,
                        Some(&alice.token),
                        &format!("/workspaces/{}/members", w.id),
                        Some(serde_json::json!({"email": people[who].email, "role": "member"})),
                    )
                    .await;
                    let h = head(&srv, w.id).await;
                    socks[who] = connect(&srv, &people[who], w.id, h).await;
                }
            }
        }
        let h = head(&srv, w.id).await;
        for (i, s) in socks.iter_mut().enumerate() {
            let before = s.cursor;
            let expected = replay_seqs(&srv, &people[i], w.id, before).await;
            let got = if expected.is_some() {
                drain(s, w.id, h).await
            } else {
                vec![]
            };
            let expected = expected.unwrap_or_default();
            assert_eq!(
                got, expected,
                "seed {seed} step {step}: socket {i} from {before} differs from replay"
            );
        }
    }
    let filtered = srv
        .state
        .hub
        .batches_filtered
        .load(std::sync::atomic::Ordering::Relaxed);
    let replayed = srv
        .state
        .hub
        .batches_replayed
        .load(std::sync::atomic::Ordering::Relaxed);
    println!("seed {seed}: {filtered} batches filtered in memory, {replayed} sent to replay");
    assert!(
        filtered > replayed * 4,
        "the in-memory path must carry most batches"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_delivery_matches_replay() {
    for seed in 1..=4 {
        run(seed).await;
    }
}
