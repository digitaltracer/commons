//! The network behind a trait, so a browser build can swap in `fetch` and browser sockets.
use std::path::Path;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite;
use uuid::Uuid;

/// Called with the bytes of an upload sent so far.
pub type Progress = Arc<dyn Fn(u64) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("HTTP {status}: {body}")]
    Status {
        status: u16,
        body: serde_json::Value,
    },
    #[error("network: {0}")]
    Network(String),
}

impl TransportError {
    pub fn status(&self) -> Option<u16> {
        match self {
            TransportError::Status { status, .. } => Some(*status),
            TransportError::Network(_) => None,
        }
    }
    /// Worth retrying: network failures, timeouts, rate limits and server errors.
    pub fn is_transient(&self) -> bool {
        match self.status() {
            None => true,
            Some(s) => s == 408 || s == 429 || s >= 500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[async_trait::async_trait]
pub trait Socket: Send {
    async fn send(&mut self, text: String) -> Result<(), TransportError>;
    /// The next text frame; `None` when the socket closed.
    async fn recv(&mut self) -> Option<Result<String, TransportError>>;
}

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
        token: Option<&str>,
        idempotency_key: Option<Uuid>,
    ) -> Result<serde_json::Value, TransportError>;

    async fn socket(&self, token: &str) -> Result<Box<dyn Socket>, TransportError>;

    /// Streams a local file to `path` as the request body.
    async fn upload(
        &self,
        path: &str,
        query: &[(&str, String)],
        file: &Path,
        token: &str,
        progress: Progress,
    ) -> Result<serde_json::Value, TransportError>;

    /// Fetches bytes and the content type.
    async fn download(&self, path: &str, token: &str) -> Result<(Vec<u8>, String), TransportError>;
}

/// Typed helpers over a [`Transport`].
pub async fn call<T: DeserializeOwned>(
    t: &dyn Transport,
    method: Method,
    path: &str,
    query: &[(&str, String)],
    body: Option<&impl Serialize>,
    token: Option<&str>,
    key: Option<Uuid>,
) -> Result<T, TransportError> {
    let body = match body {
        Some(b) => {
            Some(serde_json::to_value(b).map_err(|e| TransportError::Network(e.to_string()))?)
        }
        None => None,
    };
    let v = t.request(method, path, query, body, token, key).await?;
    serde_json::from_value(v).map_err(|e| TransportError::Network(format!("decode {path}: {e}")))
}

/// HTTPS and WebSocket over reqwest and tungstenite.
pub struct HttpTransport {
    base: String,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(server_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        HttpTransport {
            base: server_url.trim_end_matches('/').to_string(),
            client,
        }
    }
}

async fn to_result(resp: reqwest::Response) -> Result<reqwest::Response, TransportError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    Err(TransportError::Status {
        status: status.as_u16(),
        body,
    })
}

fn net(e: impl std::fmt::Display) -> TransportError {
    TransportError::Network(e.to_string())
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
        token: Option<&str>,
        key: Option<Uuid>,
    ) -> Result<serde_json::Value, TransportError> {
        let url = format!("{}/v1{path}", self.base);
        let m = match method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut req = self.client.request(m, url).query(query);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(k) = key {
            req = req.header("Idempotency-Key", k.to_string());
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = to_result(req.send().await.map_err(net)?).await?;
        let bytes = resp.bytes().await.map_err(net)?;
        if bytes.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(net)
    }

    async fn socket(&self, token: &str) -> Result<Box<dyn Socket>, TransportError> {
        let url = format!("{}/v1/socket", self.base.replacen("http", "ws", 1));
        let mut req =
            tungstenite::client::IntoClientRequest::into_client_request(url).map_err(net)?;
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().map_err(net)?,
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|e| match e {
                tungstenite::Error::Http(r) => TransportError::Status {
                    status: r.status().as_u16(),
                    body: serde_json::Value::Null,
                },
                other => net(other),
            })?;
        Ok(Box::new(WsSocket(ws)))
    }

    async fn upload(
        &self,
        path: &str,
        query: &[(&str, String)],
        file: &Path,
        token: &str,
        progress: Progress,
    ) -> Result<serde_json::Value, TransportError> {
        let f = tokio::fs::File::open(file).await.map_err(net)?;
        let len = f.metadata().await.map_err(net)?.len();
        let mut sent = 0u64;
        let stream = tokio_util::io::ReaderStream::new(f).inspect(move |chunk| {
            if let Ok(c) = chunk {
                sent += c.len() as u64;
                progress(sent);
            }
        });
        let resp = self
            .client
            .post(format!("{}/v1{path}", self.base))
            .query(query)
            .bearer_auth(token)
            .header("Content-Length", len)
            .timeout(std::time::Duration::from_secs(3600))
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(net)?;
        to_result(resp).await?.json().await.map_err(net)
    }

    async fn download(&self, path: &str, token: &str) -> Result<(Vec<u8>, String), TransportError> {
        let resp = self
            .client
            .get(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .map_err(net)?;
        let resp = to_result(resp).await?;
        let mime = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        Ok((resp.bytes().await.map_err(net)?.to_vec(), mime))
    }
}

struct WsSocket(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
);

#[async_trait::async_trait]
impl Socket for WsSocket {
    async fn send(&mut self, text: String) -> Result<(), TransportError> {
        self.0
            .send(tungstenite::Message::Text(text.into()))
            .await
            .map_err(net)
    }
    async fn recv(&mut self) -> Option<Result<String, TransportError>> {
        loop {
            match self.0.next().await? {
                Ok(tungstenite::Message::Text(t)) => return Some(Ok(t.to_string())),
                Ok(tungstenite::Message::Close(_)) => return None,
                Ok(_) => continue,
                Err(e) => return Some(Err(net(e))),
            }
        }
    }
}
