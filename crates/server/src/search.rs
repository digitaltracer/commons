//! Search (decision 12). This module is the search interface: callers use [`search`] and
//! never see how it is answered. v1 answers with PostgreSQL full-text search; the planned
//! Tantivy index for CJK replaces the body of [`search`] behind the same signature.
use crate::sqlx;
use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use proto::api::{SearchHit, SearchHitKind, SearchResults};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::{ApiError, ApiResult};

pub const MAX_QUERY_CHARS: usize = 256;
pub const MAX_LIMIT: i64 = 50;
pub const MAX_OFFSET: i64 = 1000;

pub fn routes() -> Router<AppState> {
    Router::new().route("/workspaces/{id}/search", get(handle))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// One statement does it all, so the permission check fails closed with the query: the
/// readable set is computed first, hits are drawn only from it, and ranking and snippets run
/// only on the page that survives. Readable means an open access interval (joined, or by
/// role), or a public channel for a workspace member who is not a guest.
const SEARCH_SQL: &str = r#"
WITH q AS (
    SELECT websearch_to_tsquery('commons_search', $3) AS query
), readable AS (
    SELECT c.id, c.name FROM conversations c
      JOIN workspace_members wm ON wm.workspace_id = c.workspace_id
           AND wm.user_id = $2 AND wm.left_seq IS NULL
     WHERE c.workspace_id = $1 AND (
           EXISTS (SELECT 1 FROM access_intervals ai
                    WHERE ai.conversation_id = c.id AND ai.user_id = $2 AND ai.left_seq IS NULL)
           OR (c.kind = 'public' AND wm.role <> 'guest'))
), hits AS (
    SELECT 'message' AS kind, m.conversation_id, m.id AS message_id, NULL::uuid AS file_id,
           NULL::text AS file_name, m.body AS doc, m.search AS vec, m.created_at, m.author_id,
           m.thread_root_id
      FROM messages m, q
     WHERE m.conversation_id IN (SELECT id FROM readable)
       AND NOT m.deleted AND m.search @@ q.query
    UNION ALL
    SELECT 'file_name', f.conversation_id, f.message_id, f.id, f.name, f.name, f.name_search,
           m.created_at, m.author_id, m.thread_root_id
      FROM files f JOIN messages m ON m.id = f.message_id, q
     WHERE f.conversation_id IN (SELECT id FROM readable)
       AND NOT m.deleted AND f.name_search @@ q.query
    UNION ALL
    SELECT 'file_text', f.conversation_id, f.message_id, f.id, f.name, f.extracted_text,
           f.text_search, m.created_at, m.author_id, m.thread_root_id
      FROM files f JOIN messages m ON m.id = f.message_id, q
     WHERE f.conversation_id IN (SELECT id FROM readable)
       AND NOT m.deleted AND f.text_search @@ q.query
), page AS (
    SELECT hits.*, ts_rank(hits.vec, q.query) AS rank FROM hits, q
     ORDER BY hits.created_at DESC, rank DESC, hits.kind, hits.message_id, hits.file_id
     LIMIT $4 OFFSET $5
)
SELECT page.kind, page.conversation_id, r.name AS conversation_name, page.message_id,
       page.file_id, page.file_name, page.created_at, page.author_id, page.thread_root_id,
       ts_headline('commons_search', page.doc, q.query,
                   'StartSel=' || chr(2) || ', StopSel=' || chr(3)
                   || ', MaxWords=24, MinWords=8, MaxFragments=2, FragmentDelimiter=" … "')
           AS snippet
  FROM page JOIN readable r ON r.id = page.conversation_id, q
 ORDER BY page.created_at DESC, page.rank DESC, page.kind, page.message_id, page.file_id
"#;

fn kind(s: &str) -> ApiResult<SearchHitKind> {
    Ok(match s {
        "message" => SearchHitKind::Message,
        "file_name" => SearchHitKind::FileName,
        "file_text" => SearchHitKind::FileText,
        other => return Err(ApiError::Internal(anyhow::anyhow!("bad hit kind {other}"))),
    })
}

/// Hits for `query` in what `user_id` can read now in a workspace, newest first with rank
/// as the tiebreaker. A caller who is not a current member gets an error, never hits.
pub async fn search(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
    query: &str,
    limit: i64,
    offset: i64,
) -> ApiResult<Vec<SearchHit>> {
    if crate::sync::active_role(conn, workspace_id, user_id)
        .await?
        .is_none()
    {
        return Err(ApiError::Forbidden);
    }
    let rows = sqlx::query(SEARCH_SQL)
        .bind(workspace_id)
        .bind(user_id)
        .bind(query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(SearchHit {
                kind: kind(r.try_get("kind")?)?,
                conversation_id: r.try_get("conversation_id")?,
                conversation_name: r.try_get("conversation_name")?,
                message_id: r.try_get("message_id")?,
                file_id: r.try_get("file_id")?,
                file_name: r.try_get("file_name")?,
                snippet: r.try_get("snippet")?,
                created_at_ms: crate::db::ms(r.try_get("created_at")?),
                author_id: r.try_get("author_id")?,
                thread_root_id: r.try_get("thread_root_id")?,
            })
        })
        .collect()
}

/// Search messages, file names and extracted file text in a workspace. Covers joined
/// channels and DMs, plus public channels for members who are not guests. CJK text is not
/// segmented in v1.
#[utoipa::path(get, path = "/v1/workspaces/{id}/search",
    params(("id" = Uuid, Path), ("q" = String, Query), ("limit" = Option<i64>, Query),
           ("offset" = Option<i64>, Query)),
    responses((status = 200, body = SearchResults), (status = 400), (status = 403)))]
pub async fn handle(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<SearchResults>> {
    let query = q.q.trim();
    if query.is_empty() {
        return Err(ApiError::BadRequest("search for something".into()));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(ApiError::BadRequest("that search is too long".into()));
    }
    let limit = q.limit.unwrap_or(20).clamp(1, MAX_LIMIT);
    let offset = q.offset.unwrap_or(0).clamp(0, MAX_OFFSET);
    let mut conn = state.pool.acquire().await?;
    let hits = search(&mut conn, workspace_id, auth.user_id, query, limit, offset).await?;
    Ok(Json(SearchResults { hits }))
}
