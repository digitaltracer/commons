//! Sending and reading messages.
use crate::sqlx;
use std::collections::HashMap;

use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, patch, put};
use axum::{Json, Router};
use proto::api::{EditConflict, EditMessage, MessagePage, SendMessage};
use proto::events::WorkspaceEventBody;
use proto::model::{Entity, Message, Reaction};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::conversations;
use crate::error::{ApiError, ApiResult};
use crate::idempotency::{self, Claim};
use crate::sync::{self, NewEvent};

pub const MAX_BODY_BYTES: usize = 16 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/conversations/{id}/messages", get(history).post(send))
        .route("/messages/{id}", patch(edit).delete(delete))
        .route(
            "/messages/{id}/reactions/{emoji}",
            put(react).delete(unreact),
        )
}

const MESSAGE_COLUMNS: &str = "m.id, m.workspace_id, m.conversation_id, m.author_id, m.client_id,
    m.thread_root_id, m.body, m.entities, m.content_type, m.encryption, m.attachments,
    m.revision, m.deleted, m.created_seq, m.updated_seq, m.created_at, m.edited_at";

fn row_to_message(r: &sqlx::postgres::PgRow) -> ApiResult<Message> {
    let entities: serde_json::Value = r.try_get("entities")?;
    Ok(Message {
        id: r.try_get("id")?,
        workspace_id: r.try_get("workspace_id")?,
        conversation_id: r.try_get("conversation_id")?,
        author_id: r.try_get("author_id")?,
        client_id: r.try_get("client_id")?,
        thread_root_id: r.try_get("thread_root_id")?,
        body: r.try_get("body")?,
        entities: serde_json::from_value(entities).unwrap_or_default(),
        content_type: r.try_get("content_type")?,
        encryption: r.try_get("encryption")?,
        attachments: vec![],
        reactions: vec![],
        revision: r.try_get("revision")?,
        deleted: r.try_get("deleted")?,
        created_seq: r.try_get::<Option<i64>, _>("created_seq")?.unwrap_or(0),
        updated_seq: r.try_get::<Option<i64>, _>("updated_seq")?.unwrap_or(0),
        created_at_ms: crate::db::ms(r.try_get("created_at")?),
        edited_at_ms: r
            .try_get::<Option<time::OffsetDateTime>, _>("edited_at")?
            .map(crate::db::ms),
    })
}

/// Loads messages with their reactions and attachments, in the order given.
pub async fn load_many(conn: &mut PgConnection, ids: &[Uuid]) -> ApiResult<Vec<Message>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let sql = format!("SELECT {MESSAGE_COLUMNS} FROM messages m WHERE m.id = ANY($1)");
    let rows = sqlx::query(&sql).bind(ids).fetch_all(&mut *conn).await?;
    let mut by_id = HashMap::new();
    let mut attachments = HashMap::new();
    for r in &rows {
        let m = row_to_message(r)?;
        let files: Vec<Uuid> = r.try_get("attachments")?;
        attachments.insert(m.id, files);
        by_id.insert(m.id, m);
    }
    let reactions = sqlx::query(
        "SELECT message_id, emoji, array_agg(user_id ORDER BY created_at) AS users,
                min(created_at) AS first
           FROM reactions WHERE message_id = ANY($1)
          GROUP BY message_id, emoji ORDER BY first",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    for r in reactions {
        let id: Uuid = r.try_get("message_id")?;
        if let Some(m) = by_id.get_mut(&id) {
            m.reactions.push(Reaction {
                emoji: r.try_get("emoji")?,
                user_ids: r.try_get("users")?,
            });
        }
    }
    attach_files(conn, &mut by_id, attachments).await?;
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Fills `Message::attachments` in the order each message lists them.
async fn attach_files(
    conn: &mut PgConnection,
    by_id: &mut HashMap<Uuid, Message>,
    attachments: HashMap<Uuid, Vec<Uuid>>,
) -> ApiResult<()> {
    let all: Vec<Uuid> = attachments.values().flatten().copied().collect();
    let infos = crate::files::load_infos(conn, &all).await?;
    for (message_id, ids) in attachments {
        if let Some(m) = by_id.get_mut(&message_id) {
            m.attachments = ids.iter().filter_map(|id| infos.get(id).cloned()).collect();
        }
    }
    Ok(())
}

pub async fn load_one(conn: &mut PgConnection, id: Uuid) -> ApiResult<Message> {
    load_many(conn, &[id])
        .await?
        .pop()
        .ok_or(ApiError::NotFound)
}

/// Checks entity offsets, and keeps only mentions of members of the conversation.
pub async fn validate_entities(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    conversation_id: Uuid,
    body: &str,
    entities: &[Entity],
) -> ApiResult<Vec<Entity>> {
    let mut kept = Vec::with_capacity(entities.len());
    for e in entities {
        let (start, end) = match e {
            Entity::Mention { start, end, .. }
            | Entity::ChannelLink { start, end, .. }
            | Entity::Emoji { start, end, .. } => (*start as usize, *end as usize),
        };
        if start > end
            || end > body.len()
            || !body.is_char_boundary(start)
            || !body.is_char_boundary(end)
        {
            return Err(ApiError::BadRequest(
                "entity offsets are out of range".into(),
            ));
        }
        match e {
            Entity::Mention { user_id, .. } => {
                // A mention of someone outside the conversation stays plain text: it is not
                // stored as an entity and notifies nobody.
                if !conversations::can_read_now(conn, conversation_id, *user_id).await {
                    continue;
                }
            }
            Entity::ChannelLink {
                conversation_id: target,
                ..
            } => {
                let ok: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM conversations WHERE id = $1 AND workspace_id = $2)",
                )
                .bind(target)
                .bind(workspace_id)
                .fetch_one(&mut *conn)
                .await?;
                if !ok {
                    return Err(ApiError::BadRequest("unknown channel link".into()));
                }
            }
            Entity::Emoji { name, .. } => {
                if name.is_empty() || name.len() > 64 {
                    return Err(ApiError::BadRequest("bad emoji".into()));
                }
            }
        }
        kept.push(e.clone());
    }
    Ok(kept)
}

/// Send a message. Resending with the same `client_id` returns the first result.
#[utoipa::path(post, path = "/v1/conversations/{id}/messages", params(("id" = Uuid, Path)),
    request_body = SendMessage, responses((status = 200, body = Message), (status = 403)))]
pub async fn send(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
    Json(req): Json<SendMessage>,
) -> ApiResult<Json<Message>> {
    if req.body.len() > MAX_BODY_BYTES {
        return Err(ApiError::TooLarge("message is too long".into()));
    }
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return Err(ApiError::BadRequest("message is empty".into()));
    }
    let mut tx = state.pool.begin().await?;
    if let Claim::Replay(prev) = idempotency::claim(&mut tx, auth.user_id, req.client_id).await? {
        let id: Uuid = serde_json::from_value(prev["id"].clone())
            .map_err(|_| ApiError::Conflict(serde_json::json!({"error": "key_reused"})))?;
        tx.rollback().await?;
        let mut conn = state.pool.acquire().await?;
        return Ok(Json(load_one(&mut conn, id).await?));
    }
    if !conversations::can_read_now(&mut tx, conversation_id, auth.user_id).await {
        return Err(ApiError::Forbidden);
    }
    let workspace_id: Uuid =
        sqlx::query_scalar("SELECT workspace_id FROM conversations WHERE id = $1")
            .bind(conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    if let Some(root) = req.thread_root_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE id = $1 AND conversation_id = $2
               AND thread_root_id IS NULL)",
        )
        .bind(root)
        .bind(conversation_id)
        .fetch_one(&mut *tx)
        .await?;
        if !ok {
            return Err(ApiError::BadRequest("thread root not found".into()));
        }
    }
    let entities = validate_entities(
        &mut tx,
        workspace_id,
        conversation_id,
        &req.body,
        &req.entities,
    )
    .await?;
    let attachments = crate::messages::check_attachments(
        &mut tx,
        conversation_id,
        auth.user_id,
        &req.attachments,
    )
    .await?;
    let id = Uuid::now_v7();
    let entities = serde_json::to_value(&entities).map_err(|e| ApiError::Internal(e.into()))?;
    sqlx::query(
        "INSERT INTO messages (id, workspace_id, conversation_id, author_id, client_id,
                               thread_root_id, body, entities, attachments)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(conversation_id)
    .bind(auth.user_id)
    .bind(req.client_id)
    .bind(req.thread_root_id)
    .bind(&req.body)
    .bind(entities)
    .bind(&attachments)
    .execute(&mut *tx)
    .await?;
    crate::files::link(&mut tx, id, &attachments).await?;
    let mut message = load_one(&mut tx, id).await?;
    idempotency::store(
        &mut tx,
        auth.user_id,
        req.client_id,
        &serde_json::json!({ "id": id }),
    )
    .await?;
    crate::messages::after_post(&mut tx, &message).await?;
    let seqs = sync::append(
        &mut tx,
        workspace_id,
        vec![NewEvent::conversation(WorkspaceEventBody::MessagePosted {
            message: message.clone(),
        })],
    )
    .await?;
    tx.commit().await?;
    message.created_seq = seqs[0];
    message.updated_seq = seqs[0];
    Ok(Json(message))
}

/// Validates and locks the files a message attaches: the sender's own unsent uploads to
/// this conversation.
pub async fn check_attachments(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<Vec<Uuid>> {
    crate::files::claim_for_message(conn, conversation_id, user_id, ids).await
}

/// Work queued in the same transaction as a new message: the notify job.
pub async fn after_post(conn: &mut PgConnection, message: &Message) -> ApiResult<()> {
    crate::notify::enqueue(conn, message).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    /// Only messages created before this seq (the join point of the interval being served).
    pub until_seq: Option<i64>,
    /// Paging cursor: only messages created before this seq.
    pub before_seq: Option<i64>,
    /// The access interval the page is for; echoed back so the client can discard stale pages.
    pub interval_seq: Option<i64>,
    pub limit: Option<i64>,
}

/// History for backfill, newest first, including tombstones.
#[utoipa::path(get, path = "/v1/conversations/{id}/messages", params(("id" = Uuid, Path),
    ("until_seq" = Option<i64>, Query), ("before_seq" = Option<i64>, Query),
    ("interval_seq" = Option<i64>, Query), ("limit" = Option<i64>, Query)),
    responses((status = 200, body = MessagePage), (status = 403)))]
pub async fn history(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<MessagePage>> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    if !conversations::can_read_now(&mut tx, conversation_id, auth.user_id).await {
        return Err(ApiError::Forbidden);
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let bound = q
        .until_seq
        .unwrap_or(i64::MAX)
        .min(q.before_seq.unwrap_or(i64::MAX));
    let rows = sqlx::query(
        "SELECT id, created_seq FROM messages
          WHERE conversation_id = $1 AND created_seq IS NOT NULL AND created_seq < $2
          ORDER BY created_seq DESC LIMIT $3",
    )
    .bind(conversation_id)
    .bind(bound)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get(0)).collect();
    let last_seq: Option<i64> = rows.last().map(|r| r.get(1));
    let messages = load_many(&mut tx, &ids).await?;
    tx.commit().await?;
    Ok(Json(MessagePage {
        next_before_seq: if (ids.len() as i64) < limit {
            None
        } else {
            last_seq
        },
        messages,
        interval_seq: q.interval_seq.unwrap_or(0),
    }))
}

fn idempotency_key(headers: &HeaderMap) -> Option<Uuid> {
    headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// Locks a message the caller can read. Fails toward deny.
async fn lock_readable(
    conn: &mut PgConnection,
    id: Uuid,
    user_id: Uuid,
) -> ApiResult<(Uuid, Uuid, Uuid, i64, bool)> {
    let row: Option<(Uuid, Uuid, Uuid, i64, bool)> = sqlx::query_as(
        "SELECT workspace_id, conversation_id, author_id, revision, deleted
           FROM messages WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let row = row.ok_or(ApiError::NotFound)?;
    if !conversations::can_read_now(conn, row.1, user_id).await {
        return Err(ApiError::NotFound);
    }
    Ok(row)
}

/// Edit a message. The edit names the revision it was based on; a stale base is rejected
/// with 409 and the current message, and an edit of a deleted message gets 410.
#[utoipa::path(patch, path = "/v1/messages/{id}", params(("id" = Uuid, Path)),
    request_body = EditMessage,
    responses((status = 200, body = Message), (status = 409, body = EditConflict), (status = 410, body = EditConflict)))]
pub async fn edit(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<EditMessage>,
) -> ApiResult<Json<Message>> {
    if req.body.len() > MAX_BODY_BYTES {
        return Err(ApiError::TooLarge("message is too long".into()));
    }
    let mut tx = state.pool.begin().await?;
    let key = idempotency_key(&headers);
    if let Some(k) = key
        && let Claim::Replay(prev) = idempotency::claim(&mut tx, auth.user_id, k).await?
    {
        tx.rollback().await?;
        return Ok(Json(
            serde_json::from_value(prev).map_err(|e| ApiError::Internal(e.into()))?,
        ));
    }
    let (workspace_id, conversation_id, author, revision, deleted) =
        lock_readable(&mut tx, id, auth.user_id).await?;
    if author != auth.user_id {
        return Err(ApiError::Forbidden);
    }
    if deleted || revision != req.base_revision {
        let current = load_one(&mut tx, id).await?;
        let error = if deleted { "deleted" } else { "stale" };
        let body = serde_json::to_value(EditConflict {
            error: error.into(),
            current,
        })
        .map_err(|e| ApiError::Internal(e.into()))?;
        return Err(if deleted {
            ApiError::Gone(body)
        } else {
            ApiError::Conflict(body)
        });
    }
    let entities = validate_entities(
        &mut tx,
        workspace_id,
        conversation_id,
        &req.body,
        &req.entities,
    )
    .await?;
    sqlx::query(
        "UPDATE messages SET body = $2, entities = $3, revision = revision + 1, edited_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(&req.body)
    .bind(serde_json::to_value(&entities).map_err(|e| ApiError::Internal(e.into()))?)
    .execute(&mut *tx)
    .await?;
    let mut message = load_one(&mut tx, id).await?;
    let seqs = sync::append(
        &mut tx,
        workspace_id,
        vec![NewEvent::conversation(WorkspaceEventBody::MessageEdited {
            message: message.clone(),
        })],
    )
    .await?;
    message.updated_seq = seqs[0];
    if let Some(k) = key {
        idempotency::store(
            &mut tx,
            auth.user_id,
            k,
            &serde_json::to_value(&message).unwrap_or_default(),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(message))
}

/// Delete a message (its author, or a workspace owner or admin). Delete wins over any edit,
/// and deleting twice is a no-op.
#[utoipa::path(delete, path = "/v1/messages/{id}", params(("id" = Uuid, Path)),
    responses((status = 200, body = Message)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Message>> {
    let mut tx = state.pool.begin().await?;
    let (workspace_id, _, author, _, deleted) = lock_readable(&mut tx, id, auth.user_id).await?;
    if author != auth.user_id {
        let role = sync::active_role(&mut tx, workspace_id, auth.user_id).await?;
        if !role.is_some_and(|r| r.reads_private_channels()) {
            return Err(ApiError::Forbidden);
        }
    }
    if deleted {
        let m = load_one(&mut tx, id).await?;
        tx.rollback().await?;
        return Ok(Json(m));
    }
    sqlx::query(
        "UPDATE messages SET body = '', entities = '[]', attachments = '{}', deleted = true,
                revision = revision + 1 WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM reactions WHERE message_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let mut message = load_one(&mut tx, id).await?;
    let seqs = sync::append(
        &mut tx,
        workspace_id,
        vec![NewEvent::conversation(WorkspaceEventBody::MessageDeleted {
            message: message.clone(),
        })],
    )
    .await?;
    message.updated_seq = seqs[0];
    tx.commit().await?;
    Ok(Json(message))
}

fn check_emoji(e: &str) -> ApiResult<()> {
    if e.is_empty() || e.len() > 64 || e.chars().any(char::is_whitespace) {
        return Err(ApiError::BadRequest("bad emoji".into()));
    }
    Ok(())
}

/// Add a reaction. Idempotent; there is no revision check.
#[utoipa::path(put, path = "/v1/messages/{id}/reactions/{emoji}",
    params(("id" = Uuid, Path), ("emoji" = String, Path)), responses((status = 204)))]
pub async fn react(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, emoji)): Path<(Uuid, String)>,
) -> ApiResult<axum::http::StatusCode> {
    set_reaction(&state, auth.user_id, id, emoji, true).await
}

/// Remove a reaction. Idempotent.
#[utoipa::path(delete, path = "/v1/messages/{id}/reactions/{emoji}",
    params(("id" = Uuid, Path), ("emoji" = String, Path)), responses((status = 204)))]
pub async fn unreact(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, emoji)): Path<(Uuid, String)>,
) -> ApiResult<axum::http::StatusCode> {
    set_reaction(&state, auth.user_id, id, emoji, false).await
}

async fn set_reaction(
    state: &AppState,
    user_id: Uuid,
    id: Uuid,
    emoji: String,
    add: bool,
) -> ApiResult<axum::http::StatusCode> {
    check_emoji(&emoji)?;
    let mut tx = state.pool.begin().await?;
    // Lock the message first: its row is stamped by the append below.
    let (workspace_id, conversation_id, _, _, deleted) =
        lock_readable(&mut tx, id, user_id).await?;
    if deleted {
        return Err(ApiError::Gone(
            serde_json::json!({"error": "deleted", "message": "message was deleted"}),
        ));
    }
    let changed = if add {
        sqlx::query("INSERT INTO reactions (message_id, user_id, emoji) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
    } else {
        sqlx::query("DELETE FROM reactions WHERE message_id = $1 AND user_id = $2 AND emoji = $3")
    }
    .bind(id)
    .bind(user_id)
    .bind(&emoji)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if changed {
        let body = if add {
            WorkspaceEventBody::ReactionAdded {
                conversation_id,
                message_id: id,
                user_id,
                emoji,
            }
        } else {
            WorkspaceEventBody::ReactionRemoved {
                conversation_id,
                message_id: id,
                user_id,
                emoji,
            }
        };
        sync::append(&mut tx, workspace_id, vec![NewEvent::conversation(body)]).await?;
    }
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
