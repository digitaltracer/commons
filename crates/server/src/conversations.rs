//! Conversations and access intervals (decision 10).
use crate::sqlx;
use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use proto::api::ConversationList;
use proto::events::{AccessVia, WorkspaceEventBody};
use proto::model::{Conversation, ConversationKind};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::{ApiError, ApiResult};
use crate::sync::{self, NewEvent};

pub fn routes() -> Router<AppState> {
    Router::new().route("/workspaces/{id}/conversations", get(list_mine))
}

pub fn via_str(v: AccessVia) -> &'static str {
    match v {
        AccessVia::Membership => "membership",
        AccessVia::Role => "role",
    }
}

pub fn row_to_conversation(r: &sqlx::postgres::PgRow) -> ApiResult<Conversation> {
    let kind: String = r.try_get("kind")?;
    Ok(Conversation {
        id: r.try_get("id")?,
        workspace_id: r.try_get("workspace_id")?,
        kind: ConversationKind::parse(&kind)
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("bad kind {kind}")))?,
        name: r.try_get("name")?,
        dm_user_ids: r
            .try_get::<Option<Vec<Uuid>>, _>("dm_user_ids")?
            .unwrap_or_default(),
    })
}

const CONVERSATION_COLUMNS: &str = "c.id, c.workspace_id, c.kind, c.name,
    CASE WHEN c.kind IN ('dm', 'group_dm') THEN
        (SELECT array_agg(DISTINCT ai.user_id ORDER BY ai.user_id) FROM access_intervals ai
          WHERE ai.conversation_id = c.id AND ai.via = 'membership' AND ai.left_seq IS NULL)
    END AS dm_user_ids";

pub async fn load(conn: &mut PgConnection, id: Uuid) -> ApiResult<Conversation> {
    let sql = format!("SELECT {CONVERSATION_COLUMNS} FROM conversations c WHERE c.id = $1");
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(ApiError::NotFound)?;
    row_to_conversation(&row)
}

/// Conversations `user` can read now in a workspace.
pub async fn readable(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
) -> ApiResult<Vec<Conversation>> {
    let sql = format!(
        "SELECT {CONVERSATION_COLUMNS} FROM conversations c
          WHERE c.workspace_id = $1 AND EXISTS (
              SELECT 1 FROM access_intervals ai WHERE ai.conversation_id = c.id
                 AND ai.user_id = $2 AND ai.left_seq IS NULL)
          ORDER BY c.created_at"
    );
    let rows = sqlx::query(&sql)
        .bind(workspace_id)
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter().map(row_to_conversation).collect()
}

/// True when `user` holds an open access interval and is a current workspace member.
/// Any error is a denial.
pub async fn can_read_now(conn: &mut PgConnection, conversation_id: Uuid, user_id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1 FROM access_intervals ai
              JOIN conversations c ON c.id = ai.conversation_id
              JOIN workspace_members wm ON wm.workspace_id = c.workspace_id
                   AND wm.user_id = ai.user_id AND wm.left_seq IS NULL
             WHERE ai.conversation_id = $1 AND ai.user_id = $2 AND ai.left_seq IS NULL)",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await
    .unwrap_or(false)
}

async fn has_open(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
    except: Option<&'static str>,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM access_intervals WHERE conversation_id = $1
           AND user_id = $2 AND left_seq IS NULL AND ($3::text IS NULL OR via <> $3))",
    )
    .bind(conversation_id)
    .bind(user_id)
    .bind(except)
    .fetch_one(&mut *conn)
    .await?)
}

/// Opens an access interval. Returns the `member_added` event, or `None` if one was open.
/// The event's seq becomes the interval's `joined_seq` when it is appended.
pub async fn grant(
    conn: &mut PgConnection,
    conversation: &Conversation,
    user_id: Uuid,
    via: AccessVia,
) -> ApiResult<Option<NewEvent>> {
    let via_s = via_str(via);
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO access_intervals (conversation_id, user_id, via) VALUES ($1, $2, $3)
         ON CONFLICT (conversation_id, user_id, via) WHERE left_seq IS NULL DO NOTHING
         RETURNING id",
    )
    .bind(conversation.id)
    .bind(user_id)
    .bind(via_s)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id.map(|id| {
        NewEvent::conversation(WorkspaceEventBody::MemberAdded {
            conversation: conversation.clone(),
            user_id,
            via,
            has_access: true,
        })
        .about(user_id)
        .interval(id)
    }))
}

/// Closes the open interval for `via`. `has_access` reports whether another interval remains.
pub async fn revoke(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
    via: AccessVia,
) -> ApiResult<Option<NewEvent>> {
    let via_s = via_str(via);
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM access_intervals WHERE conversation_id = $1 AND user_id = $2
           AND via = $3 AND left_seq IS NULL FOR UPDATE",
    )
    .bind(conversation_id)
    .bind(user_id)
    .bind(via_s)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(id) = id else { return Ok(None) };
    let has_access = has_open(conn, conversation_id, user_id, Some(via_s)).await?;
    Ok(Some(
        NewEvent::conversation(WorkspaceEventBody::MemberRemoved {
            conversation_id,
            user_id,
            via,
            has_access,
        })
        .about(user_id)
        .interval(id),
    ))
}

pub async fn create(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    kind: ConversationKind,
    name: Option<&str>,
    dm_key: Option<&str>,
    is_default: bool,
) -> ApiResult<Conversation> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, kind, name, dm_key, is_default)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(kind.as_str())
    .bind(name)
    .bind(dm_key)
    .bind(is_default)
    .execute(&mut *conn)
    .await?;
    Ok(Conversation {
        id,
        workspace_id,
        kind,
        name: name.map(str::to_string),
        dm_user_ids: vec![],
    })
}

#[utoipa::path(get, path = "/v1/workspaces/{id}/conversations", params(("id" = Uuid, Path)),
    responses((status = 200, body = ConversationList)))]
pub async fn list_mine(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<ConversationList>> {
    let mut conn = state.pool.acquire().await?;
    if sync::active_role(&mut conn, workspace_id, auth.user_id)
        .await?
        .is_none()
    {
        return Err(ApiError::Forbidden);
    }
    let conversations = readable(&mut conn, workspace_id, auth.user_id).await?;
    Ok(Json(ConversationList { conversations }))
}
