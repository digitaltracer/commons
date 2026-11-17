//! Step 3 load test (goose): 500 clients on one workspace.
//!
//! Run with: `ulimit -n 10240; cargo test --release -p server --test load -- --ignored --nocapture`
//!
//! Phases:
//! - A, paced: 500 senders at about 50 sends per second while 500 sockets receive;
//!   reports send-to-receive latency (p50/p95/p99) and the POST latency.
//! - B, unpaced: 500 senders as fast as they can; reports sustained commits per second.
//! - C, burst: 1,000 concurrent writes; reports the time until every socket has received all of them.
//!
//! By default Postgres runs in testcontainers on tmpfs, so commit fsync is nearly free.
//! Set `COMMONS_LOAD_DATABASE_URL` to an empty, migratable database to measure real disk commits.
mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use futures::{SinkExt, StreamExt};
use goose::config::GooseConfiguration;
use goose::prelude::*;
use proto::events::{ServerFrame, WorkspaceEventBody};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

const CLIENTS: usize = 500;

static TOKENS: OnceLock<Vec<String>> = OnceLock::new();
static PATH: OnceLock<String> = OnceLock::new();

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

async fn send(user: &mut GooseUser) -> TransactionResult {
    let tokens = TOKENS.get().unwrap();
    let token = &tokens[user.weighted_users_index % tokens.len()];
    let body = serde_json::json!({"client_id": Uuid::now_v7(), "body": format!("t={}", now_us())});
    let rb = user
        .get_request_builder(&GooseMethod::Post, PATH.get().unwrap())?
        .bearer_auth(token)
        .json(&body);
    let req = GooseRequest::builder()
        .method(GooseMethod::Post)
        .path(PATH.get().unwrap().as_str())
        .set_request_builder(rb)
        .build();
    user.request(req).await?;
    Ok(())
}

async fn attack(host: &str, users: usize, seconds: usize, wait: Option<Duration>) -> GooseMetrics {
    let mut scenario = scenario!("send").register_transaction(transaction!(send));
    if let Some(w) = wait {
        scenario = scenario.set_wait_time(w, w).unwrap();
    }
    GooseAttack::initialize_with_config(GooseConfiguration::default())
        .unwrap()
        .register_scenario(scenario)
        .set_default(GooseDefault::Host, host)
        .unwrap()
        .set_default(GooseDefault::Users, users)
        .unwrap()
        .set_default(GooseDefault::IncreaseRate, "250")
        .unwrap()
        .set_default(GooseDefault::RunTime, seconds)
        .unwrap()
        .set_default(GooseDefault::Quiet, 1)
        .unwrap()
        .set_default(GooseDefault::NoPrintMetrics, true)
        .unwrap()
        .execute()
        .await
        .unwrap()
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn post_summary(m: &GooseMetrics) -> String {
    m.requests
        .values()
        .map(|r| {
            format!(
                "{} requests, {} failed, mean {:.1} ms, max {:.1} ms",
                r.raw_data.counter,
                r.fail_count,
                r.raw_data.total_time_us as f64 / 1000.0 / r.raw_data.counter.max(1) as f64,
                r.raw_data.maximum_time_us as f64 / 1000.0
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test: run explicitly with a raised file-descriptor limit"]
async fn five_hundred_clients() {
    let srv = match std::env::var("COMMONS_LOAD_DATABASE_URL") {
        Ok(url) => {
            let mut c = server::testing::test_config(url);
            c.job_workers = 2;
            server::testing::spawn_with(c).await
        }
        Err(_) => server::testing::spawn(|c| c.job_workers = 2).await,
    };
    let owner = register(&srv, "owner").await;
    let ws = workspace(&srv, &owner, &[]).await;
    let mut people = vec![];
    for chunk in (0..CLIENTS).collect::<Vec<_>>().chunks(25) {
        let names: Vec<String> = chunk.iter().map(|i| format!("u{i}")).collect();
        let regs = names.iter().map(|n| register(&srv, n));
        people.extend(futures::future::join_all(regs).await);
    }
    for p in &people {
        ok(
            &srv,
            reqwest::Method::POST,
            &owner,
            &format!("/workspaces/{}/members", ws.id),
            Some(serde_json::json!({"email": p.email, "role": "member"})),
        )
        .await;
    }
    let general: Uuid = {
        let v = ok(
            &srv,
            reqwest::Method::GET,
            &owner,
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
    TOKENS
        .set(people.iter().map(|p| p.token.clone()).collect())
        .unwrap();
    PATH.set(format!("/v1/conversations/{general}/messages"))
        .unwrap();

    // 500 receiving sockets.
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::default();
    let cursors: Arc<Vec<AtomicI64>> = Arc::new((0..CLIENTS).map(|_| AtomicI64::new(0)).collect());
    let start = head(&srv, ws.id).await;
    for (i, p) in people.iter().enumerate() {
        let mut req = srv.ws_url.clone().into_client_request().unwrap();
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", p.token).parse().unwrap(),
        );
        let (mut sock, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let user_since: i64 = server::sqlx::query_scalar("SELECT seq FROM users WHERE id = $1")
            .bind(p.id)
            .fetch_one(srv.pool())
            .await
            .unwrap();
        let hello = serde_json::json!({"type": "hello", "user_since": user_since,
            "workspaces": [{"workspace_id": ws.id, "since": start}]});
        sock.send(WsMessage::Text(hello.to_string().into()))
            .await
            .unwrap();
        let latencies = latencies.clone();
        let cursors = cursors.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = sock.next().await {
                let WsMessage::Text(t) = msg else { continue };
                let Ok(ServerFrame::Workspace {
                    events, next_since, ..
                }) = serde_json::from_str(&t)
                else {
                    continue;
                };
                let now = now_us();
                let mut batch = vec![];
                for e in events {
                    if let WorkspaceEventBody::MessagePosted { message } = e.body
                        && let Some(t) = message
                            .body
                            .strip_prefix("t=")
                            .and_then(|v| v.parse::<i64>().ok())
                    {
                        batch.push((now - t) as f64 / 1000.0);
                    }
                }
                latencies.lock().unwrap().extend(batch);
                cursors[i].store(next_since, Ordering::Relaxed);
            }
        });
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let host = srv.base_url.clone();

    // Phase A: about 50 sends per second.
    latencies.lock().unwrap().clear();
    let a = attack(&host, CLIENTS, 30, Some(Duration::from_secs(10))).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut lat = latencies.lock().unwrap().clone();
    let sends_a = a
        .requests
        .values()
        .map(|r| r.raw_data.counter)
        .sum::<usize>();
    println!(
        "A paced: {sends_a} sends in 30 s ({:.0}/s); POST: {}",
        sends_a as f64 / 30.0,
        post_summary(&a)
    );
    println!(
        "A send-to-receive over {} deliveries: p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        lat.len(),
        percentile(&mut lat, 0.5),
        percentile(&mut lat, 0.95),
        percentile(&mut lat, 0.99),
        lat.last().copied().unwrap_or(0.0)
    );

    // Phase B: unpaced, sustained commits per second on one workspace.
    let before = head(&srv, ws.id).await;
    let t0 = Instant::now();
    let b = attack(&host, CLIENTS, 15, None).await;
    let secs = t0.elapsed().as_secs_f64();
    let after = head(&srv, ws.id).await;
    println!(
        "B unpaced: {} commits in {secs:.1} s = {:.0} commits/s; POST: {}",
        after - before,
        (after - before) as f64 / secs,
        post_summary(&b)
    );
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Phase C: a 1,000-write burst and the time until every socket has drained it.
    let target_start = head(&srv, ws.id).await;
    let client = reqwest::Client::new();
    let t0 = Instant::now();
    let writes = (0..1000).map(|i| {
        let client = client.clone();
        let token = people[i % CLIENTS].token.clone();
        let url = format!("{}/v1/conversations/{general}/messages", srv.base_url);
        async move {
            client
                .post(url)
                .bearer_auth(token)
                .json(&serde_json::json!({"client_id": Uuid::now_v7(), "body": "burst"}))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
        }
    });
    let ok_writes = futures::future::join_all(writes)
        .await
        .into_iter()
        .filter(|b| *b)
        .count();
    let committed = t0.elapsed();
    let target = target_start + ok_writes as i64;
    let mut drained = None;
    while t0.elapsed() < Duration::from_secs(120) {
        if cursors.iter().all(|c| c.load(Ordering::Relaxed) >= target) {
            drained = Some(t0.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let behind = cursors
        .iter()
        .filter(|c| c.load(Ordering::Relaxed) < target)
        .count();
    match drained {
        Some(d) => println!(
            "C burst: {ok_writes} writes committed in {:.0} ms; every socket had all of them after {:.0} ms",
            committed.as_secs_f64() * 1000.0,
            d.as_secs_f64() * 1000.0
        ),
        None => println!(
            "C burst: {ok_writes} writes committed in {:.0} ms; {behind} sockets had not drained after 120 s",
            committed.as_secs_f64() * 1000.0
        ),
    }
    println!(
        "slow consumers dropped: {}",
        srv.state.hub.dropped_slow.load(Ordering::Relaxed)
    );
}
