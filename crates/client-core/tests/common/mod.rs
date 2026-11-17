#![allow(dead_code)]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use client_core::account::{Account, AccountConfig, AccountEvent};
use client_core::platform::RecordingNotifier;
use client_core::store::{SqliteStore, Storage};
use client_core::transport::HttpTransport;
use proto::api::SessionResponse;
use proto::model::{User, Workspace};
use server::testing::TestServer;
use uuid::Uuid;

pub struct Person {
    pub token: String,
    pub user: User,
}

pub async fn post<T: serde::de::DeserializeOwned>(
    srv: &TestServer,
    token: Option<&str>,
    path: &str,
    body: serde_json::Value,
) -> T {
    let mut req = reqwest::Client::new()
        .post(format!("{}/v1{path}", srv.base_url))
        .json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "POST {path}: {status} {text}");
    serde_json::from_str(if text.is_empty() { "null" } else { &text }).unwrap()
}

pub async fn request(
    srv: &TestServer,
    method: reqwest::Method,
    token: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut req = reqwest::Client::new()
        .request(method, format!("{}/v1{path}", srv.base_url))
        .bearer_auth(token);
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

pub async fn register(srv: &TestServer, name: &str) -> Person {
    let s: SessionResponse = post(
        srv,
        None,
        "/auth/register",
        serde_json::json!({
            "email": format!("{name}-{}@example.org", Uuid::now_v7().simple()),
            "password": "correct horse battery",
            "display_name": name,
        }),
    )
    .await;
    Person {
        token: s.token,
        user: s.user,
    }
}

pub async fn workspace(srv: &TestServer, owner: &Person, members: &[&Person]) -> Workspace {
    let w: Workspace = post(
        srv,
        Some(&owner.token),
        "/workspaces",
        serde_json::json!({"name": "Acme"}),
    )
    .await;
    for m in members {
        let _: serde_json::Value = post(
            srv,
            Some(&owner.token),
            &format!("/workspaces/{}/members", w.id),
            serde_json::json!({"email": m.user.email, "role": "member"}),
        )
        .await;
    }
    w
}

pub fn account(srv: &TestServer, p: &Person, dir: &Path) -> Account {
    account_with_store(
        srv,
        p,
        Arc::new(SqliteStore::open(&dir.join("cache.sqlite")).unwrap()),
    )
}

pub fn account_with_store(srv: &TestServer, p: &Person, store: Arc<dyn Storage>) -> Account {
    account_full(srv, p, store, Arc::new(RecordingNotifier::default()))
}

pub fn account_full(
    srv: &TestServer,
    p: &Person,
    store: Arc<dyn Storage>,
    notifier: Arc<RecordingNotifier>,
) -> Account {
    Account::new(
        AccountConfig {
            id: Uuid::now_v7(),
            server_url: srv.base_url.clone(),
            user: p.user.clone(),
            data_dir: std::env::temp_dir(),
        },
        Arc::new(HttpTransport::new(&srv.base_url)),
        store,
        Some(p.token.clone()),
        notifier,
        Arc::new(|_: Uuid, _: AccountEvent| {}),
    )
}

pub fn memory() -> Arc<dyn Storage> {
    Arc::new(SqliteStore::in_memory().unwrap())
}

/// Runs every queued notify job now.
pub async fn run_jobs(srv: &TestServer) {
    while let Some(job) = server::jobs::claim(srv.pool(), std::time::Duration::from_secs(30))
        .await
        .unwrap()
    {
        server::jobs::run_one(&srv.state, job).await;
    }
}

pub async fn eventually<F: FnMut() -> bool>(what: &str, mut f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

pub fn general(acc: &Account, ws: Uuid) -> Uuid {
    acc.store
        .conversations(ws)
        .unwrap()
        .into_iter()
        .find(|(c, _)| c.name.as_deref() == Some("general"))
        .map(|(c, _)| c.id)
        .expect("general channel")
}
