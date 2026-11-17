//! Sending and reading messages.
use crate::sqlx;
use std::collections::HashMap;

use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use proto::api::{MessagePage, SendMessage};
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
    Router::new().route("/conversations/{id}/messages", get(history).post(send))
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

/// Fills `Message::attachments`. Files arrive in step 4; until then this is a no-op.
async fn attach_files(
    _conn: &mut PgConnection,
    _by_id: &mut HashMap<Uuid, Message>,
    _attachments: HashMap<Uuid, Vec<Uuid>>,
) -> ApiResult<()> {
    Ok(())
}

pub async fn load_one(conn: &mut PgConnection, id: Uuid) -> ApiResult<Message> {
    load_many(conn, &[id])
        .await?
        .pop()
        .ok_or(ApiError::NotFound)
}

/// Checks entity offsets and that every mention targets a member of the conversation.
pub async fn validate_entities(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    conversation_id: Uuid,
    body: &str,
    entities: &[Entity],
) -> ApiResult<()> {
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
                if !conversations::can_read_now(conn, conversation_id, *user_id).await {
                    return Err(ApiError::BadRequest(
                        "mentions must target members of the conversation".into(),
                    ));
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
    }
    Ok(())
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
    validate_entities(
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
    let entities = serde_json::to_value(&req.entities).map_err(|e| ApiError::Internal(e.into()))?;
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

/// Validates attachment ids. Files arrive in step 4; until then attachments are refused.
pub async fn check_attachments(
    _conn: &mut PgConnection,
    _conversation_id: Uuid,
    _user_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<Vec<Uuid>> {
    if !ids.is_empty() {
        return Err(ApiError::BadRequest(
            "attachments are not supported yet".into(),
        ));
    }
    Ok(vec![])
}

/// Work queued in the same transaction as a new message. Notifications arrive in step 3.
pub async fn after_post(_conn: &mut PgConnection, _message: &Message) -> ApiResult<()> {
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
