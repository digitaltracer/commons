#![allow(dead_code)]
use proto::api::SessionResponse;
use proto::model::{Conversation, Workspace};
use server::testing::TestServer;
use uuid::Uuid;

pub struct Person {
    pub token: String,
    pub id: Uuid,
    pub email: String,
}

pub async fn call(
    srv: &TestServer,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut req = reqwest::Client::new().request(method, format!("{}/v1{path}", srv.base_url));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

pub async fn ok(
    srv: &TestServer,
    method: reqwest::Method,
    p: &Person,
    path: &str,
    body: Option<serde_json::Value>,
) -> serde_json::Value {
    let (s, v) = call(srv, method, Some(&p.token), path, body).await;
    assert!((200..300).contains(&s), "{path}: {s} {v}");
    v
}

pub async fn register(srv: &TestServer, name: &str) -> Person {
    let email = format!("{name}-{}@example.org", Uuid::now_v7().simple());
    let (s, v) = call(
        srv,
        reqwest::Method::POST,
        None,
        "/auth/register",
        Some(serde_json::json!({"email": email, "password": "correct horse battery", "display_name": name})),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let r: SessionResponse = serde_json::from_value(v).unwrap();
    Person {
        token: r.token,
        id: r.user.id,
        email,
    }
}

pub async fn workspace(srv: &TestServer, owner: &Person, members: &[&Person]) -> Workspace {
    let w = ok(
        srv,
        reqwest::Method::POST,
        owner,
        "/workspaces",
        Some(serde_json::json!({"name": "W"})),
    )
    .await;
    let w: Workspace = serde_json::from_value(w).unwrap();
    for m in members {
        ok(
            srv,
            reqwest::Method::POST,
            owner,
            &format!("/workspaces/{}/members", w.id),
            Some(serde_json::json!({"email": m.email, "role": "member"})),
        )
        .await;
    }
    w
}

pub async fn channel(
    srv: &TestServer,
    p: &Person,
    ws: Uuid,
    name: &str,
    private: bool,
) -> Conversation {
    let kind = if private { "private" } else { "public" };
    let v = ok(
        srv,
        reqwest::Method::POST,
        p,
        &format!("/workspaces/{ws}/channels"),
        Some(serde_json::json!({"name": name, "kind": kind})),
    )
    .await;
    serde_json::from_value(v).unwrap()
}

pub async fn post(srv: &TestServer, p: &Person, conv: Uuid, body: &str) -> serde_json::Value {
    ok(
        srv,
        reqwest::Method::POST,
        p,
        &format!("/conversations/{conv}/messages"),
        Some(serde_json::json!({"client_id": Uuid::now_v7(), "body": body})),
    )
    .await
}

pub async fn head(srv: &TestServer, ws: Uuid) -> i64 {
    server::sqlx::query_scalar("SELECT seq FROM workspaces WHERE id = $1")
        .bind(ws)
        .fetch_one(srv.pool())
        .await
        .unwrap()
}

/// Posts in a scratch channel until the workspace head reaches `target`.
pub async fn pad_to(srv: &TestServer, p: &Person, scratch: Uuid, ws: Uuid, target: i64) {
    while head(srv, ws).await < target {
        post(srv, p, scratch, "pad").await;
    }
    assert_eq!(head(srv, ws).await, target, "padding overshot");
}

pub async fn replay(
    srv: &TestServer,
    p: &Person,
    ws: Uuid,
    since: i64,
) -> (u16, serde_json::Value) {
    call(
        srv,
        reqwest::Method::GET,
        Some(&p.token),
        &format!("/sync/workspaces/{ws}?since={since}"),
        None,
    )
    .await
}
