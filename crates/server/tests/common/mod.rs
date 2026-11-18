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

/// A test socket: frames arrive on `rx`; `rx` closes when the server closes the socket.
pub struct Socket {
    pub rx: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    pub tx: tokio::sync::mpsc::UnboundedSender<String>,
}

pub async fn user_seq(srv: &TestServer, user: Uuid) -> i64 {
    server::sqlx::query_scalar("SELECT seq FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(srv.pool())
        .await
        .unwrap()
}

/// Opens a socket with `token` and says hello with these cursors.
pub async fn open_socket(
    srv: &TestServer,
    token: &str,
    user_since: i64,
    workspaces: &[(Uuid, i64)],
) -> Socket {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = srv.ws_url.clone().into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut sink, mut stream) = socket.split();
    let ws: Vec<_> = workspaces
        .iter()
        .map(|(w, s)| serde_json::json!({"workspace_id": w, "since": s}))
        .collect();
    let hello = serde_json::json!({"type": "hello", "user_since": user_since, "workspaces": ws});
    sink.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(m)) = stream.next().await {
            if let Message::Text(t) = m
                && tx.send(serde_json::from_str(&t).unwrap()).is_err()
            {
                return;
            }
        }
    });
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(t) = out_rx.recv().await {
            if sink.send(Message::Text(t.into())).await.is_err() {
                return;
            }
        }
    });
    Socket { rx, tx: out_tx }
}

impl Socket {
    /// The next frame that matches `pred`, or `None` on timeout or close.
    pub async fn wait_for(
        &mut self,
        within: std::time::Duration,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> Option<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(f)) if pred(&f) => return Some(f),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }

    /// Every frame that arrives within `within`. Stops early if the socket closes.
    pub async fn collect(&mut self, within: std::time::Duration) -> Vec<serde_json::Value> {
        let mut out = vec![];
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(f)) => out.push(f),
                _ => return out,
            }
        }
    }

    /// True once the server has closed the socket.
    pub async fn closed_within(&mut self, within: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(_)) => continue,
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }
}

pub async fn login(srv: &TestServer, p: &Person) -> String {
    let (s, v) = call(
        srv,
        reqwest::Method::POST,
        None,
        "/auth/login",
        Some(serde_json::json!({"email": p.email, "password": "correct horse battery"})),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    v["token"].as_str().unwrap().to_string()
}
