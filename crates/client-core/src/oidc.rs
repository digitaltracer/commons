//! The app's side of OIDC sign-in: a loopback listener (RFC 8252) that the browser returns
//! to, and the exchange of its handoff code for a session or a link.
use std::time::Duration;

use base64::Engine;
use proto::api::{OidcExchange, OidcExchanged, OidcStart, OidcStarted};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::transport::{Method, Transport, call};

/// How long the browser has to come back.
pub const TIMEOUT: Duration = Duration::from_secs(300);

/// Why a provider sign-in didn't finish.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// The server or provider refused it; the message is for the person.
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("The sign-in took too long. Try again.")]
    TimedOut,
    #[error("The sign-in was cancelled.")]
    Cancelled,
    #[error("Couldn't open the browser: {0}")]
    Browser(String),
}

fn verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn page(status: &str, title: &str, text: &str) -> String {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{t}</title>\
         <body style=\"font:16px system-ui,sans-serif;max-width:28rem;margin:4rem auto;padding:0 1rem\">\
         <h1>{t}</h1><p>{p}</p>",
        t = escape(title),
        p = escape(text)
    );
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\n\
         referrer-policy: no-referrer\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Waits for the browser's `GET /oidc?…` and answers it with a page. Other requests (a
/// favicon) get a 404 and the wait goes on.
async fn wait_for_return(listener: &TcpListener) -> std::io::Result<Vec<(String, String)>> {
    loop {
        let (mut conn, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let mut n = 0;
        let head = loop {
            let read = match tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf[n..]))
                .await
            {
                Ok(Ok(r)) => r,
                _ => break None,
            };
            if read == 0 {
                break None;
            }
            n += read;
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") || n == buf.len() {
                break Some(String::from_utf8_lossy(&buf[..n]).into_owned());
            }
        };
        let Some(head) = head else { continue };
        let target = head
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("GET "))
            .and_then(|l| l.split(' ').next())
            .unwrap_or("");
        let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).ok();
        let Some(u) = parsed.filter(|u| u.path() == "/oidc") else {
            let _ = conn
                .write_all(page("404 Not Found", "Not found", "").as_bytes())
                .await;
            continue;
        };
        let pairs: Vec<(String, String)> = u.query_pairs().into_owned().collect();
        let reply = match pairs.iter().find(|(k, _)| k == "message") {
            Some((_, m)) => page("200 OK", "Sign-in didn't finish", m),
            None => page(
                "200 OK",
                "Back to Commons",
                "You can close this tab. Commons is finishing your sign-in.",
            ),
        };
        let _ = conn.write_all(reply.as_bytes()).await;
        let _ = conn.shutdown().await;
        return Ok(pairs);
    }
}

/// Runs one provider flow on `transport`'s server: starts it, opens the browser at the
/// provider, waits for the browser to return to the loopback listener, and exchanges the
/// code. `token` is the session for a link. Nothing is stored here.
pub async fn run(
    transport: &dyn Transport,
    token: Option<&str>,
    invite: Option<String>,
    link: bool,
    open: impl FnOnce(&str) -> anyhow::Result<()>,
    cancel: &tokio::sync::Notify,
    timeout: Duration,
) -> anyhow::Result<OidcExchanged> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let verifier = verifier();
    let started: OidcStarted = call(
        transport,
        Method::Post,
        "/auth/oidc/start",
        &[],
        Some(&OidcStart {
            return_to: format!("http://127.0.0.1:{port}/oidc"),
            code_challenge: challenge(&verifier),
            invite,
            link,
        }),
        token,
        None,
    )
    .await?;
    open(&started.authorize_url).map_err(|e| OidcError::Browser(e.to_string()))?;
    let pairs = tokio::select! {
        r = wait_for_return(&listener) => r?,
        _ = tokio::time::sleep(timeout) => return Err(OidcError::TimedOut.into()),
        _ = cancel.notified() => return Err(OidcError::Cancelled.into()),
    };
    drop(listener);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    if let Some(code) = get("error") {
        return Err(OidcError::Refused {
            message: get("message").unwrap_or_else(|| "The sign-in didn't finish.".into()),
            code,
        }
        .into());
    }
    let code = get("code").ok_or_else(|| OidcError::Refused {
        code: "no_code".into(),
        message: "The browser came back without a sign-in code. Try again.".into(),
    })?;
    Ok(call(
        transport,
        Method::Post,
        "/auth/oidc/exchange",
        &[],
        Some(&OidcExchange {
            code,
            code_verifier: verifier,
        }),
        token,
        None,
    )
    .await?)
}
