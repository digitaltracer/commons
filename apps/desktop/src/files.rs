//! The `commons-file` protocol (decision 4). The webview loads attachments by reference; the
//! core fetches them with the account's session, so the webview never holds a token, and
//! the server checks access on every load.
use std::sync::Arc;

use client_core::Core;
use client_core::account::SyncError;
use tauri::http::{Response, header};
use uuid::Uuid;

pub const SCHEME: &str = "commons-file";

/// Types the webview may render. Anything else is served as an opaque download.
const INLINE: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// `/{account_id}/{file_id}` or `/{account_id}/{file_id}/thumbnail`.
pub fn parse(path: &str) -> Option<(Uuid, Uuid, bool)> {
    let mut parts = path.trim_start_matches('/').split('/');
    let account = parts.next()?.parse().ok()?;
    let file = parts.next()?.parse().ok()?;
    let thumbnail = match parts.next() {
        None => false,
        Some("thumbnail") => true,
        Some(_) => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((account, file, thumbnail))
}

pub fn status(code: u16) -> Response<Vec<u8>> {
    let mut r = Response::new(Vec::new());
    *r.status_mut() = tauri::http::StatusCode::from_u16(code)
        .unwrap_or(tauri::http::StatusCode::INTERNAL_SERVER_ERROR);
    r
}

pub async fn respond(core: Arc<Core>, path: &str) -> Response<Vec<u8>> {
    let Some((account_id, file_id, thumbnail)) = parse(path) else {
        return status(400);
    };
    let Ok(account) = core.account(account_id) else {
        return status(404);
    };
    match account.fetch_file(file_id, thumbnail).await {
        Ok((bytes, mime)) => {
            let inline = INLINE.contains(&mime.as_str());
            let (mime, disposition) = if inline {
                (mime, "inline")
            } else {
                ("application/octet-stream".to_string(), "attachment")
            };
            Response::builder()
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_DISPOSITION, disposition)
                .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
                .header(header::CONTENT_SECURITY_POLICY, "sandbox")
                .header(header::CACHE_CONTROL, "no-store")
                .body(bytes)
                .unwrap_or_else(|_| status(500))
        }
        Err(SyncError::SignedOut) => status(401),
        Err(SyncError::Transport(e)) if matches!(e.status(), Some(403 | 404)) => status(404),
        Err(e) => {
            tracing::warn!(file = %file_id, error = %e, "file load failed");
            status(502)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_and_thumbnail_paths() {
        let (a, f) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(parse(&format!("/{a}/{f}")), Some((a, f, false)));
        assert_eq!(parse(&format!("/{a}/{f}/thumbnail")), Some((a, f, true)));
        assert_eq!(parse(&format!("/{a}/{f}/other")), None);
        assert_eq!(parse(&format!("/{a}/{f}/thumbnail/x")), None);
        assert_eq!(parse(&format!("/{a}")), None);
        assert_eq!(parse("/not-a-uuid/x"), None);
    }
}
