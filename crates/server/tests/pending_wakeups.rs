//! Wakeups the socket set aside while chaining batches are never dropped: they are handled
//! before the socket waits again, and in their original order. Each test runtime is
//! single-threaded, so wakeups handed to the hub back to back are queued together.
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use server::feed::Batch;
use server::hub::Ephemeral;
use uuid::Uuid;

fn empty_batch(ws: Uuid, from: i64) -> Arc<Batch> {
    Arc::new(Batch {
        workspace_id: ws,
        from,
        to: from + 1,
        head: from + 1,
        events: vec![],
    })
}

fn presence(ws: Uuid, user: Uuid) -> Ephemeral {
    Ephemeral {
        workspace_id: ws,
        frame: proto::events::ServerFrame::Presence {
            user_id: user,
            online: true,
        },
    }
}

/// The repair tick is due on nearly every turn of the socket loop, so it competes with a
/// set-aside wakeup each round. Each round's batch is for its own scratch workspace, because
/// the synthetic batch moves that cursor past the real head.
#[tokio::test]
async fn a_set_aside_wakeup_is_always_handled() {
    let srv = server::testing::spawn(|c| c.repair_interval = Duration::from_millis(5)).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let stay = workspace(&srv, &alice, &[&bob]).await;
    let mut cursors = vec![(stay.id, head(&srv, stay.id).await)];
    let mut scratch = vec![];
    for _ in 0..20 {
        let w = workspace(&srv, &alice, &[&bob]).await;
        let h = head(&srv, w.id).await;
        cursors.push((w.id, h));
        scratch.push((w.id, h));
    }
    let mut s = open_socket(&srv, &bob.token, user_seq(&srv, bob.id).await, &cursors).await;
    s.collect(Duration::from_millis(500)).await;

    let mut want = vec![];
    for (w, h) in scratch {
        let marker = Uuid::now_v7();
        want.push(marker.to_string());
        srv.state.hub.batch(empty_batch(w, h));
        srv.state.hub.ephemeral(presence(stay.id, marker));
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let frames = s.collect(Duration::from_secs(1)).await;
    let got: Vec<String> = frames
        .iter()
        .filter(|f| f["type"] == "presence")
        .map(|f| f["user_id"].as_str().unwrap().to_string())
        .filter(|u| want.contains(u))
        .collect();
    assert_eq!(
        got, want,
        "every presence frame queued behind a batch arrives"
    );
}
