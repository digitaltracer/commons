//! Step 5 through the core: search returns view models titled from the cache, marks public
//! channels the user has not joined, and never shows a channel the user cannot read.
mod common;

use common::*;
use proto::model::Conversation;

#[tokio::test(flavor = "multi_thread")]
async fn search_hits_become_titled_views() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let mk = |name: &'static str, kind: &'static str| {
        let srv = &srv;
        let alice = &alice;
        async move {
            let c: Conversation = post(
                srv,
                Some(&alice.token),
                &format!("/workspaces/{}/channels", ws.id),
                serde_json::json!({"name": name, "kind": kind}),
            )
            .await;
            c
        }
    };
    let lobby = mk("lobby", "public").await;
    let vault = mk("vault", "private").await;
    let say = |conv: uuid::Uuid, body: &'static str| {
        let srv = &srv;
        let alice = &alice;
        async move {
            let _: serde_json::Value = post(
                srv,
                Some(&alice.token),
                &format!("/conversations/{conv}/messages"),
                serde_json::json!({"client_id": uuid::Uuid::now_v7(), "body": body}),
            )
            .await;
        }
    };
    let general = {
        let a = account_with_store(&srv, &alice, memory());
        a.sync_once().await.unwrap();
        general(&a, ws.id)
    };
    say(general, "kumquat for everyone").await;
    say(lobby.id, "kumquat in the lobby").await;
    say(vault.id, "kumquat in the vault").await;

    let b = account_with_store(&srv, &bob, memory());
    b.sync_once().await.unwrap();
    let hits = b.search(ws.id, "kumquat", 0).await.unwrap();
    let mut seen: Vec<(String, bool)> = hits
        .iter()
        .map(|h| (h.conversation_title.clone(), h.joined))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("#general".to_string(), true),
            ("#lobby".to_string(), false)
        ]
    );
    let lobby_hit = hits.iter().find(|h| h.conversation_id == lobby.id).unwrap();
    assert!(
        lobby_hit
            .snippet
            .iter()
            .any(|p| p.hit && p.text == "kumquat")
    );
}
