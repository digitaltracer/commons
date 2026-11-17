//! Files (decision 13): streaming uploads capped by quotas, object storage through
//! `object_store`, downloads proxied and authorized on every request, and thumbnails and text
//! extraction as jobs.
use crate::sqlx;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use proto::api::UploadResponse;
use proto::events::WorkspaceEventBody;
use proto::model::FileInfo;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::conversations;
use crate::error::{ApiError, ApiResult};
use crate::jobs::{self, Job};
use crate::sqlx::{PgConnection, Row};
use crate::sync::{self, NewEvent};
use crate::{AppState, Config};

pub const THUMBNAIL_JOB: &str = "file_thumbnail";
pub const EXTRACT_JOB: &str = "file_extract";
pub const MAX_ATTACHMENTS: usize = 10;
/// Extracted text kept for search, in bytes. Postgres caps a tsvector at 1 MB.
pub const MAX_EXTRACTED_BYTES: usize = 256 * 1024;
const THUMBNAIL_PX: u32 = 320;
const MAX_THUMBNAIL_SOURCE: i64 = 40 << 20;
const MAX_EXTRACT_SOURCE: i64 = 50 << 20;
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);
/// Multipart parts in flight per upload; with 5 MB parts this bounds upload memory.
const UPLOAD_CONCURRENCY: usize = 4;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/conversations/{id}/files",
            post(upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/files/{id}", get(download))
        .route("/files/{id}/thumbnail", get(download_thumbnail))
}

/// The configured store: S3-compatible when a bucket is set, otherwise local disk.
pub fn open_store(config: &Config) -> anyhow::Result<Arc<dyn ObjectStore>> {
    Ok(match &config.s3_bucket {
        Some(bucket) => Arc::new(
            object_store::aws::AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .build()?,
        ),
        None => {
            std::fs::create_dir_all(&config.files_dir)?;
            Arc::new(object_store::local::LocalFileSystem::new_with_prefix(
                &config.files_dir,
            )?)
        }
    })
}

fn object_key(id: Uuid) -> StorePath {
    StorePath::from(format!("files/{id}"))
}

fn thumbnail_key(id: Uuid) -> StorePath {
    StorePath::from(format!("thumbnails/{id}"))
}

fn store_err(e: object_store::Error) -> ApiError {
    match e {
        object_store::Error::NotFound { .. } => ApiError::NotFound,
        other => ApiError::Internal(other.into()),
    }
}

const FILE_COLUMNS: &str = "f.id, f.conversation_id, f.name, f.size, f.mime, f.has_thumbnail";

fn row_to_file(r: &sqlx::postgres::PgRow) -> ApiResult<FileInfo> {
    Ok(FileInfo {
        id: r.try_get("id")?,
        conversation_id: r.try_get("conversation_id")?,
        name: r.try_get("name")?,
        size: r.try_get("size")?,
        mime: r.try_get("mime")?,
        has_thumbnail: r.try_get("has_thumbnail")?,
    })
}

pub async fn load_infos(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> ApiResult<HashMap<Uuid, FileInfo>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!("SELECT {FILE_COLUMNS} FROM files f WHERE f.id = ANY($1)");
    let rows = sqlx::query(&sql).bind(ids).fetch_all(&mut *conn).await?;
    rows.iter()
        .map(|r| row_to_file(r).map(|f| (f.id, f)))
        .collect()
}

/// Locks the files a new message attaches. Each must be the sender's own upload to this
/// conversation and not yet attached to a message.
pub async fn claim_for_message(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<Vec<Uuid>> {
    let mut wanted: Vec<Uuid> = Vec::with_capacity(ids.len());
    for id in ids {
        if !wanted.contains(id) {
            wanted.push(*id);
        }
    }
    if wanted.len() > MAX_ATTACHMENTS {
        return Err(ApiError::BadRequest(format!(
            "a message carries at most {MAX_ATTACHMENTS} files"
        )));
    }
    if wanted.is_empty() {
        return Ok(wanted);
    }
    let found: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = ANY($1) AND conversation_id = $2 AND uploader_id = $3
            AND message_id IS NULL ORDER BY id FOR UPDATE",
    )
    .bind(&wanted)
    .bind(conversation_id)
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    if found.len() != wanted.len() {
        return Err(ApiError::BadRequest(
            "an attachment is unknown, not yours, or already sent".into(),
        ));
    }
    Ok(wanted)
}

/// Records which message carries the files; from then on readers of the conversation may
/// download them.
pub async fn link(conn: &mut PgConnection, message_id: Uuid, ids: &[Uuid]) -> ApiResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE files SET message_id = $1 WHERE id = ANY($2)")
        .bind(message_id)
        .bind(ids)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn used_bytes(conn: &mut PgConnection, user_id: Uuid) -> ApiResult<u64> {
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(size), 0)::bigint FROM files WHERE uploader_id = $1",
    )
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(used.max(0) as u64)
}

fn clean_name(name: &str) -> ApiResult<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let n: String = base.chars().filter(|c| !c.is_control()).collect();
    let n = n.trim();
    if n.is_empty() || n == "." || n == ".." {
        return Err(ApiError::BadRequest("the file needs a name".into()));
    }
    Ok(n.chars().take(255).collect())
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub name: String,
}

fn too_large(limit: u64, used: u64, quota: u64) -> ApiError {
    if used >= quota || quota - used < limit {
        ApiError::TooLarge("this upload would exceed your storage quota".into())
    } else {
        ApiError::TooLarge(format!("files are limited to {limit} bytes"))
    }
}

/// Upload a file to a conversation. The request body is the file's bytes, streamed to the
/// store; the file becomes visible only once complete. Attach it by id when sending.
#[utoipa::path(post, path = "/v1/conversations/{id}/files",
    params(("id" = Uuid, Path), ("name" = String, Query)),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses((status = 200, body = UploadResponse), (status = 403), (status = 413)))]
pub async fn upload(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<UploadResponse>> {
    let name = clean_name(&q.name)?;
    let declared: Option<u64> = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let quota = state.config.user_quota_bytes;
    let max = state.config.max_file_bytes;
    // No pool connection is held while the body streams.
    let used = {
        let mut conn = state.pool.acquire().await?;
        if !conversations::can_read_now(&mut conn, conversation_id, auth.user_id).await {
            return Err(ApiError::Forbidden);
        }
        used_bytes(&mut conn, auth.user_id).await?
    };
    let limit = max.min(quota.saturating_sub(used));
    if declared.is_some_and(|n| n > limit) || limit == 0 {
        return Err(too_large(max, used, quota));
    }

    let id = Uuid::now_v7();
    let key = object_key(id);
    let mut writer = WriteMultipart::new(state.files.put_multipart(&key).await.map_err(store_err)?);
    let mut size: u64 = 0;
    let mut stream = body.into_data_stream();
    let streamed: ApiResult<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| ApiError::BadRequest("the upload was interrupted".into()))?;
            size += chunk.len() as u64;
            if size > limit {
                return Err(too_large(max, used, quota));
            }
            writer
                .wait_for_capacity(UPLOAD_CONCURRENCY)
                .await
                .map_err(store_err)?;
            writer.put(chunk);
        }
        Ok(())
    }
    .await;
    if let Err(e) = streamed {
        let _ = writer.abort().await;
        return Err(e);
    }
    writer.finish().await.map_err(store_err)?;

    let mime = mime_guess::from_path(&name)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let finalized = finalize(
        &state,
        auth.user_id,
        conversation_id,
        id,
        &name,
        &mime,
        size,
    )
    .await;
    match finalized {
        Ok(file) => Ok(Json(UploadResponse { file })),
        Err(e) => {
            if let Err(d) = state.files.delete(&key).await {
                tracing::warn!(file = %id, error = %d, "orphaned upload not removed");
            }
            Err(e)
        }
    }
}

/// Inserts the row for a complete object, re-checking access and the quota under a lock on
/// the uploader, and queues the derived-content jobs.
async fn finalize(
    state: &AppState,
    user_id: Uuid,
    conversation_id: Uuid,
    id: Uuid,
    name: &str,
    mime: &str,
    size: u64,
) -> ApiResult<FileInfo> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if !conversations::can_read_now(&mut tx, conversation_id, user_id).await {
        return Err(ApiError::Forbidden);
    }
    let used = used_bytes(&mut tx, user_id).await?;
    if used + size > state.config.user_quota_bytes {
        return Err(too_large(
            state.config.max_file_bytes,
            used,
            state.config.user_quota_bytes,
        ));
    }
    sqlx::query(
        "INSERT INTO files (id, workspace_id, conversation_id, uploader_id, name, mime, size)
         SELECT $1, c.workspace_id, c.id, $3, $4, $5, $6 FROM conversations c WHERE c.id = $2",
    )
    .bind(id)
    .bind(conversation_id)
    .bind(user_id)
    .bind(name)
    .bind(mime)
    .bind(size as i64)
    .execute(&mut *tx)
    .await?;
    if thumbnail_format(mime).is_some() && (size as i64) <= MAX_THUMBNAIL_SOURCE {
        jobs::enqueue(&mut tx, THUMBNAIL_JOB, serde_json::json!({ "file_id": id })).await?;
    }
    jobs::enqueue(&mut tx, EXTRACT_JOB, serde_json::json!({ "file_id": id })).await?;
    tx.commit().await?;
    Ok(FileInfo {
        id,
        conversation_id,
        name: name.to_string(),
        size: size as i64,
        mime: mime.to_string(),
        has_thumbnail: false,
    })
}

/// A file `user` may read now. Fails toward deny: the conversation must be readable now, and
/// the file must be on a message that still exists, or be the caller's own unsent upload.
pub async fn readable(
    conn: &mut PgConnection,
    id: Uuid,
    user_id: Uuid,
) -> ApiResult<Option<FileInfo>> {
    let sql = format!(
        "SELECT {FILE_COLUMNS} FROM files f LEFT JOIN messages m ON m.id = f.message_id
          WHERE f.id = $1 AND (
              (f.message_id IS NOT NULL AND m.deleted = false)
              OR (f.message_id IS NULL AND f.uploader_id = $2))"
    );
    let Some(row) = sqlx::query(&sql)
        .bind(id)
        .bind(user_id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    let file = row_to_file(&row)?;
    if !conversations::can_read_now(conn, file.conversation_id, user_id).await {
        return Ok(None);
    }
    Ok(Some(file))
}

/// `filename*` per RFC 5987, so any name survives the header.
fn disposition(inline: bool, name: &str) -> String {
    let mut enc = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    let kind = if inline { "inline" } else { "attachment" };
    format!("{kind}; filename*=UTF-8''{enc}")
}

async fn serve(state: &AppState, user_id: Uuid, id: Uuid, thumbnail: bool) -> ApiResult<Response> {
    let file = {
        let mut conn = state.pool.acquire().await?;
        readable(&mut conn, id, user_id).await?
    }
    .ok_or(ApiError::NotFound)?;
    if thumbnail && !file.has_thumbnail {
        return Err(ApiError::NotFound);
    }
    let key = if thumbnail {
        thumbnail_key(id)
    } else {
        object_key(id)
    };
    let got = state.files.get(&key).await.map_err(store_err)?;
    let len = got.meta.size;
    let (mime, inline) = if thumbnail {
        ("image/png".to_string(), true)
    } else {
        let inline = thumbnail_format(&file.mime).is_some();
        (file.mime.clone(), inline)
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CONTENT_DISPOSITION, disposition(inline, &file.name))
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_SECURITY_POLICY, "sandbox")
        .body(Body::from_stream(got.into_stream()))
        .map_err(|e| ApiError::Internal(e.into()))
}

/// Download a file. Access is checked on every request; a download already streaming when
/// access is revoked finishes.
#[utoipa::path(get, path = "/v1/files/{id}", params(("id" = Uuid, Path)),
    responses((status = 200, content_type = "application/octet-stream"), (status = 404)))]
pub async fn download(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    serve(&state, auth.user_id, id, false).await
}

/// A PNG thumbnail of an image file, under the same access check as the file.
#[utoipa::path(get, path = "/v1/files/{id}/thumbnail", params(("id" = Uuid, Path)),
    responses((status = 200, content_type = "image/png"), (status = 404)))]
pub async fn download_thumbnail(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    serve(&state, auth.user_id, id, true).await
}

// ---- jobs ----

fn thumbnail_format(mime: &str) -> Option<image::ImageFormat> {
    Some(match mime {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/gif" => image::ImageFormat::Gif,
        "image/webp" => image::ImageFormat::WebP,
        _ => return None,
    })
}

fn make_thumbnail(bytes: &[u8], format: image::ImageFormat) -> anyhow::Result<Vec<u8>> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 << 20);
    reader.limits(limits);
    let thumb = reader.decode()?.thumbnail(THUMBNAIL_PX, THUMBNAIL_PX);
    let mut out = Vec::new();
    thumb.write_to(Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

fn file_id(payload: &serde_json::Value) -> anyhow::Result<Uuid> {
    Ok(serde_json::from_value(payload["file_id"].clone())?)
}

/// The thumbnail job. Once stored, a file already on a message is announced with
/// `file_updated`; the row lock orders this against a send attaching the file.
pub async fn thumbnail(state: &AppState, payload: &serde_json::Value) -> anyhow::Result<()> {
    let id = file_id(payload)?;
    let row: Option<(String, i64, bool)> =
        sqlx::query_as("SELECT mime, size, has_thumbnail FROM files WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((mime, size, done)) = row else {
        return Ok(());
    };
    let Some(format) = thumbnail_format(&mime) else {
        return Ok(());
    };
    if done || size > MAX_THUMBNAIL_SOURCE {
        return Ok(());
    }
    let bytes = state.files.get(&object_key(id)).await?.bytes().await?;
    let png = match tokio::task::spawn_blocking(move || make_thumbnail(&bytes, format)).await? {
        Ok(png) => png,
        Err(e) => {
            // An undecodable image keeps no thumbnail; retrying will not help.
            tracing::info!(file = %id, error = %e, "no thumbnail");
            return Ok(());
        }
    };
    state.files.put(&thumbnail_key(id), png.into()).await?;
    let mut tx = state.pool.begin().await?;
    let message: Option<Option<Uuid>> = sqlx::query_scalar(
        "UPDATE files SET has_thumbnail = true WHERE id = $1 AND NOT has_thumbnail
         RETURNING message_id",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(Some(message_id)) = message {
        let live: Option<Uuid> =
            sqlx::query_scalar("SELECT workspace_id FROM messages WHERE id = $1 AND NOT deleted")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(workspace_id) = live {
            let file = load_infos(&mut tx, &[id])
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .remove(&id)
                .ok_or_else(|| anyhow::anyhow!("file vanished"))?;
            sync::append(
                &mut tx,
                workspace_id,
                vec![NewEvent::conversation(WorkspaceEventBody::FileUpdated {
                    file,
                })],
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Text,
    Pdf,
}

const SOURCE_EXTENSIONS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "rst",
    "adoc",
    "org",
    "csv",
    "tsv",
    "log",
    "json",
    "jsonl",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "xml",
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "sql",
    "graphql",
    "proto",
    "rs",
    "py",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "go",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "hpp",
    "hh",
    "java",
    "kt",
    "kts",
    "scala",
    "swift",
    "m",
    "mm",
    "rb",
    "php",
    "pl",
    "pm",
    "lua",
    "r",
    "jl",
    "dart",
    "zig",
    "nim",
    "ex",
    "exs",
    "erl",
    "hrl",
    "hs",
    "ml",
    "mli",
    "fs",
    "fsx",
    "cs",
    "vb",
    "clj",
    "cljs",
    "elm",
    "vue",
    "svelte",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmake",
    "mk",
    "gradle",
    "tf",
    "nix",
    "dockerfile",
    "diff",
    "patch",
    "tex",
    "bib",
];

/// Plain text, Markdown, source code and PDF are extracted; other files are found by name.
pub fn text_kind(name: &str, mime: &str) -> Option<TextKind> {
    if mime == "application/pdf" {
        return Some(TextKind::Pdf);
    }
    let lower = name.to_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or(&lower);
    let known_name = matches!(
        lower.as_str(),
        "makefile" | "dockerfile" | "readme" | "license" | "changelog"
    );
    if mime.starts_with("text/") || known_name || SOURCE_EXTENSIONS.contains(&ext) {
        return Some(TextKind::Text);
    }
    None
}

/// Text for the search index: no NULs (Postgres text cannot hold them), capped in size.
pub fn clean_text(text: &str) -> String {
    let mut out: String = text.chars().filter(|c| *c != '\0').collect();
    if out.len() > MAX_EXTRACTED_BYTES {
        let mut end = MAX_EXTRACTED_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

fn extract_text(kind: TextKind, bytes: &[u8]) -> anyhow::Result<Option<String>> {
    match kind {
        TextKind::Text => {
            if bytes.iter().take(8192).any(|b| *b == 0) {
                return Ok(None);
            }
            Ok(Some(clean_text(&String::from_utf8_lossy(bytes))))
        }
        TextKind::Pdf => {
            let text = pdf_extract::extract_text_from_mem(bytes)?;
            Ok(Some(clean_text(&text)))
        }
    }
}

async fn set_text_state(state: &AppState, id: Uuid, s: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE files SET text_state = $2 WHERE id = $1")
        .bind(id)
        .bind(s)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// The extraction job. It is bounded in input size, output size and time, and it is
/// idempotent, so a lease that expires under a killed worker simply runs it again. A failure
/// never touches the file itself, so the download is unaffected.
pub async fn extract(state: &AppState, job: &Job) -> anyhow::Result<()> {
    let id = file_id(&job.payload)?;
    let row: Option<(String, String, i64, String)> =
        sqlx::query_as("SELECT name, mime, size, text_state FROM files WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((name, mime, size, text_state)) = row else {
        return Ok(());
    };
    if text_state != "pending" {
        return Ok(());
    }
    let Some(kind) = text_kind(&name, &mime).filter(|_| size <= MAX_EXTRACT_SOURCE) else {
        return set_text_state(state, id, "none").await;
    };
    let result: anyhow::Result<Option<String>> = async {
        let bytes = state.files.get(&object_key(id)).await?.bytes().await?;
        let work = tokio::task::spawn_blocking(move || extract_text(kind, &bytes));
        tokio::time::timeout(EXTRACT_TIMEOUT, work)
            .await
            .map_err(|_| anyhow::anyhow!("extraction timed out"))?
            .map_err(|e| anyhow::anyhow!("extraction crashed: {e}"))?
    }
    .await;
    match result {
        Ok(Some(text)) => {
            sqlx::query(
                "UPDATE files SET extracted_text = $2, text_state = 'done'
                  WHERE id = $1 AND text_state = 'pending'",
            )
            .bind(id)
            .bind(text)
            .execute(&state.pool)
            .await?;
            Ok(())
        }
        Ok(None) => set_text_state(state, id, "none").await,
        Err(e) => {
            if job.attempts >= job.max_attempts {
                set_text_state(state, id, "failed").await?;
            }
            Err(e)
        }
    }
}
