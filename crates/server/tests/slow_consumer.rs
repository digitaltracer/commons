//! Step 2 check: a stalled socket is dropped and server memory stays flat.
//! One test per binary, so the RSS reading is not disturbed by other tests.
mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use futures::SinkExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap()
        / 1024.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_socket_is_dropped_and_memory_stays_flat() {
    let srv = server::testing::spawn(|c| {
        c.send_timeout = Duration::from_secs(1);
        c.connection_queue = 16;
    })
    .await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let general: proto::model::Conversation = {
        let v = ok(
            &srv,
            reqwest::Method::GET,
            &bob,
            &format!("/workspaces/{}/conversations", ws.id),
            None,
        )
        .await;
        serde_json::from_value(v["conversations"][0].clone()).unwrap()
    };

    // Bob connects, says hello, then never reads again.
    let mut req = srv.ws_url.clone().into_client_request().unwrap();
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", bob.token).parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let hello = serde_json::json!({"type": "hello", "user_since": 0,
        "workspaces": [{"workspace_id": ws.id, "since": head(&srv, ws.id).await}]});
    socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(srv.state.hub.connections(bob.id), 1);

    let baseline = rss_mb();
    let body = "x".repeat(8 * 1024);
    let mut peak = baseline;
    for i in 0..1500 {
        post(&srv, &alice, general.id, &body).await;
        if i % 100 == 0 {
            peak = peak.max(rss_mb());
        }
    }
    let mut dropped = false;
    for _ in 0..100 {
        if srv.state.hub.connections(bob.id) == 0 {
            dropped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    peak = peak.max(rss_mb());
    assert!(dropped, "the stalled socket was not disconnected");
    assert!(srv.state.hub.dropped_slow.load(Ordering::Relaxed) >= 1);
    let growth = peak - baseline;
    println!(
        "rss baseline {baseline:.0} MB, peak {peak:.0} MB, growth {growth:.0} MB for 12 MB of events"
    );
    assert!(growth < 64.0, "server memory grew by {growth:.0} MB");
    drop(socket);
}
