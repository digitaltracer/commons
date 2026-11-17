//! Step 5 checks: hits and snippets come only from channels the user can read now (joined
//! ones, plus public channels for anyone who is not a guest), and the filter fails closed.
mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::*;
use proto::api::{SNIPPET_START, SNIPPET_STOP, SearchHitKind, SearchResults};
use reqwest::Method;
use server::testing::TestServer;
use uuid::Uuid;

async fn search(srv: &TestServer, p: &Person, ws: Uuid, q: &str) -> (u16, Option<SearchResults>) {
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/workspaces/{ws}/search", srv.base_url))
        .query(&[("q", q)])
        .bearer_auth(&p.token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.ok())
}

async fn hits(srv: &TestServer, p: &Person, ws: Uuid, q: &str) -> SearchResults {
    let (s, r) = search(srv, p, ws, q).await;
    assert_eq!(s, 200);
    r.unwrap()
}

/// Conversations with any hit, and every snippet, for `q`.
async fn seen(srv: &TestServer, p: &Person, ws: Uuid, q: &str) -> (BTreeSet<Uuid>, String) {
    let r = hits(srv, p, ws, q).await;
    let convs = r.hits.iter().map(|h| h.conversation_id).collect();
    let text = r
        .hits
        .iter()
        .map(|h| h.snippet.clone())
        .collect::<Vec<_>>()
        .join("\n");
    (convs, text)
}

async fn attach(srv: &TestServer, p: &Person, conv: Uuid, name: &str, bytes: &[u8], body: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/conversations/{conv}/files", srv.base_url))
        .query(&[("name", name)])
        .bearer_auth(&p.token)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    ok(
        srv,
        Method::POST,
        p,
        &format!("/conversations/{conv}/messages"),
        Some(
            serde_json::json!({"client_id": Uuid::now_v7(), "body": body,
                                "attachments": [v["file"]["id"]]}),
        ),
    )
    .await;
}

async fn run_jobs(srv: &TestServer) {
    while let Some(job) = server::jobs::claim(srv.pool(), Duration::from_secs(30))
        .await
        .unwrap()
    {
        server::jobs::run_one(&srv.state, job).await;
    }
}

async fn add_to(srv: &TestServer, by: &Person, conv: Uuid, who: &Person) {
    ok(
        srv,
        Method::POST,
        by,
        &format!("/conversations/{conv}/members"),
        Some(serde_json::json!({"user_id": who.id})),
    )
    .await;
}

async fn join_ws(srv: &TestServer, owner: &Person, ws: Uuid, who: &Person, role: &str) {
    ok(
        srv,
        Method::POST,
        owner,
        &format!("/workspaces/{ws}/members"),
        Some(serde_json::json!({"email": who.email, "role": role})),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn private_text_never_reaches_non_members() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let eve = register(&srv, "eve").await;
    let gus = register(&srv, "gus").await;
    let dana = register(&srv, "dana").await;
    let ws = workspace(&srv, &alice, &[&bob, &eve]).await.id;
    join_ws(&srv, &alice, ws, &gus, "guest").await;
    join_ws(&srv, &alice, ws, &dana, "admin").await;

    let secret = channel(&srv, &alice, ws, "secret", true).await.id;
    add_to(&srv, &alice, secret, &bob).await;
    let open = channel(&srv, &alice, ws, "open", false).await.id;
    post(&srv, &alice, secret, "the zebra launch is friday").await;
    attach(
        &srv,
        &alice,
        secret,
        "zebra-roadmap.md",
        b"inside: zebra pricing",
        "",
    )
    .await;
    post(&srv, &alice, open, "a public zebra note").await;
    run_jobs(&srv).await;

    // Bob reads both: the private message, file name, file text, and the public channel.
    let r = hits(&srv, &bob, ws, "zebra").await;
    let kinds: BTreeSet<_> = r
        .hits
        .iter()
        .filter(|h| h.conversation_id == secret)
        .map(|h| format!("{:?}", h.kind))
        .collect();
    assert_eq!(kinds.len(), 3, "{:?}", r.hits);
    assert!(r.hits.iter().any(|h| h.conversation_id == open));
    let file_hit = r
        .hits
        .iter()
        .find(|h| h.kind == SearchHitKind::FileText)
        .unwrap();
    assert_eq!(file_hit.file_name.as_deref(), Some("zebra-roadmap.md"));
    assert_eq!(file_hit.conversation_name.as_deref(), Some("secret"));

    // Eve is a member who never joined either channel: public yes, private never.
    let (convs, text) = seen(&srv, &eve, ws, "zebra").await;
    assert_eq!(convs, BTreeSet::from([open]));
    for leaked in ["launch", "roadmap", "pricing"] {
        assert!(!text.contains(leaked), "eve saw {leaked}: {text}");
    }
    assert!(seen(&srv, &eve, ws, "pricing").await.0.is_empty());
    assert!(seen(&srv, &eve, ws, "roadmap").await.0.is_empty());

    // A guest sees only what they joined, not unjoined public channels.
    assert!(seen(&srv, &gus, ws, "zebra").await.0.is_empty());
    add_to(&srv, &alice, open, &gus).await;
    assert_eq!(
        seen(&srv, &gus, ws, "zebra").await.0,
        BTreeSet::from([open])
    );

    // An admin reads the private channel through the role-derived interval.
    assert!(seen(&srv, &dana, ws, "zebra").await.0.contains(&secret));

    // Removal from the channel takes its text out of the next search.
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/conversations/{secret}/members/{}", bob.id),
        None,
    )
    .await;
    let (convs, text) = seen(&srv, &bob, ws, "zebra").await;
    assert_eq!(convs, BTreeSet::from([open]));
    assert!(
        !text.contains("launch") && !text.contains("pricing"),
        "{text}"
    );

    // Losing the admin role closes that interval too.
    ok(
        &srv,
        Method::PATCH,
        &alice,
        &format!("/workspaces/{ws}/members/{}", dana.id),
        Some(serde_json::json!({"role": "member"})),
    )
    .await;
    assert!(!seen(&srv, &dana, ws, "zebra").await.0.contains(&secret));

    // Removal from the workspace refuses the search outright.
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/workspaces/{ws}/members/{}", bob.id),
        None,
    )
    .await;
    assert_eq!(search(&srv, &bob, ws, "zebra").await.0, 403);
    assert_eq!(search(&srv, &dana, Uuid::now_v7(), "zebra").await.0, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn results_are_newest_first_with_marked_snippets() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await.id;
    let c = channel(&srv, &alice, ws, "notes", false).await.id;
    let first = post(&srv, &alice, c, "Meet at the café on Tuesday").await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let gone = post(&srv, &alice, c, "cafe plans, scrapped").await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let last = post(&srv, &alice, c, "CAFE again tomorrow").await;
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/messages/{}", gone["id"].as_str().unwrap()),
        None,
    )
    .await;

    let r = hits(&srv, &alice, ws, "cafe").await;
    let ids: Vec<String> = r
        .hits
        .iter()
        .map(|h| h.message_id.unwrap().to_string())
        .collect();
    // Accents and case fold; deleted messages are gone; newest first.
    assert_eq!(
        ids,
        vec![last["id"].as_str().unwrap(), first["id"].as_str().unwrap()]
    );
    let snippet = &r.hits[1].snippet;
    assert!(
        snippet.contains(&format!("{SNIPPET_START}café{SNIPPET_STOP}")),
        "{snippet:?}"
    );
    // Phrase and exclusion syntax.
    assert_eq!(hits(&srv, &alice, ws, "\"cafe again\"").await.hits.len(), 1);
    assert_eq!(hits(&srv, &alice, ws, "cafe -tuesday").await.hits.len(), 1);
    // Bad input is refused, not guessed at.
    assert_eq!(search(&srv, &alice, ws, "   ").await.0, 400);
    assert_eq!(search(&srv, &alice, ws, &"x".repeat(300)).await.0, 400);
    // Punctuation-only queries match nothing rather than erroring.
    assert!(hits(&srv, &alice, ws, "!!! &&").await.hits.is_empty());
}
