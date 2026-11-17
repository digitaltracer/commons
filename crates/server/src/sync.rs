//! The durable event streams (decision 9) and their replay.
//!
//! Each change writes its `events` row in the same transaction as the change. The seq comes
//! from `UPDATE workspaces SET seq = seq + n ... RETURNING` inside the final statement of the
//! transaction, so the workspace row lock is held only until commit and seq order is commit
//! order: a reader can never see seq k+1 before k has committed.
use crate::sqlx;
use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use proto::events::{WorkspaceEvent, WorkspaceEventBody, WorkspaceReplay};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::{ApiError, ApiResult};

pub const MAX_PAGE: i64 = 500;

pub fn routes() -> Router<AppState> {
    Router::new().route("/sync/workspaces/{id}", get(replay_workspace))
}

/// Who may receive a workspace event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Every current member of the workspace.
    Workspace = 0,
    /// Users whose access interval contains the seq and who can still read the conversation.
    Conversation = 1,
    /// Only `subject_user_id`.
    Subject = 2,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub body: WorkspaceEventBody,
    pub audience: Audience,
    /// The user an event is about; always delivered to them.
    pub subject_user_id: Option<Uuid>,
    /// The access interval a `member_added`/`member_removed` opens or closes.
    pub interval_id: Option<i64>,
}

impl NewEvent {
    pub fn conversation(body: WorkspaceEventBody) -> Self {
        NewEvent {
            body,
            audience: Audience::Conversation,
            subject_user_id: None,
            interval_id: None,
        }
    }
    pub fn workspace(body: WorkspaceEventBody) -> Self {
        NewEvent {
            body,
            audience: Audience::Workspace,
            subject_user_id: None,
            interval_id: None,
        }
    }
    pub fn about(mut self, user: Uuid) -> Self {
        self.subject_user_id = Some(user);
        self
    }
    pub fn interval(mut self, id: i64) -> Self {
        self.interval_id = Some(id);
        self
    }
}

fn message_id(body: &WorkspaceEventBody) -> Option<Uuid> {
    match body {
        WorkspaceEventBody::MessagePosted { message }
        | WorkspaceEventBody::MessageEdited { message }
        | WorkspaceEventBody::MessageDeleted { message } => Some(message.id),
        WorkspaceEventBody::ReactionAdded { message_id, .. }
        | WorkspaceEventBody::ReactionRemoved { message_id, .. } => Some(*message_id),
        _ => None,
    }
}

/// Appends events to a workspace stream. Call this as the last statement before commit.
///
/// One statement allocates the seqs, inserts the rows, stamps the seqs onto the messages,
/// access intervals and memberships the events describe, and queues the NOTIFY wakeup,
/// which Postgres delivers only if the transaction commits.
pub async fn append(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    events: Vec<NewEvent>,
) -> ApiResult<Vec<i64>> {
    if events.is_empty() {
        return Ok(vec![]);
    }
    let n = events.len() as i64;
    let mut kinds = Vec::with_capacity(events.len());
    let mut audiences = Vec::with_capacity(events.len());
    let mut convs = Vec::with_capacity(events.len());
    let mut msgs = Vec::with_capacity(events.len());
    let mut subjects = Vec::with_capacity(events.len());
    let mut intervals = Vec::with_capacity(events.len());
    let mut payloads = Vec::with_capacity(events.len());
    for e in &events {
        kinds.push(e.body.kind().to_string());
        audiences.push(e.audience as i16);
        convs.push(e.body.conversation_id());
        msgs.push(message_id(&e.body));
        subjects.push(e.subject_user_id);
        intervals.push(e.interval_id);
        payloads.push(serde_json::to_value(&e.body).map_err(|e| ApiError::Internal(e.into()))?);
    }
    let rows = sqlx::query(
        r#"
        WITH s AS (
            UPDATE workspaces SET seq = seq + $2 WHERE id = $1 RETURNING seq
        ), t AS (
            SELECT * FROM unnest($3::text[], $4::smallint[], $5::uuid[], $6::uuid[], $7::uuid[],
                                 $8::bigint[], $9::jsonb[]) WITH ORDINALITY
                AS t(kind, audience, conversation_id, message_id, subject_user_id, interval_id,
                     payload, ord)
        ), e AS (
            INSERT INTO events (workspace_id, seq, kind, audience, conversation_id, message_id,
                                subject_user_id, interval_id, payload)
            SELECT $1, s.seq - $2 + t.ord, t.kind, t.audience, t.conversation_id, t.message_id,
                   t.subject_user_id, t.interval_id, t.payload
            FROM s, t
            RETURNING seq, kind, message_id, subject_user_id, interval_id
        ), m AS (
            UPDATE messages
               SET updated_seq = e.seq, created_seq = COALESCE(messages.created_seq, e.seq)
              FROM e WHERE messages.id = e.message_id
            RETURNING 1
        ), io AS (
            UPDATE access_intervals SET joined_seq = e.seq
              FROM e WHERE access_intervals.id = e.interval_id AND e.kind = 'member_added'
            RETURNING 1
        ), ic AS (
            UPDATE access_intervals SET left_seq = e.seq
              FROM e WHERE access_intervals.id = e.interval_id AND e.kind = 'member_removed'
            RETURNING 1
        ), wj AS (
            UPDATE workspace_members SET joined_seq = e.seq
              FROM e WHERE workspace_members.workspace_id = $1
               AND workspace_members.user_id = e.subject_user_id
               AND e.kind = 'workspace_member_joined'
            RETURNING 1
        ), wl AS (
            UPDATE workspace_members SET left_seq = e.seq
              FROM e WHERE workspace_members.workspace_id = $1
               AND workspace_members.user_id = e.subject_user_id
               AND e.kind = 'workspace_member_left'
            RETURNING 1
        ), notify AS (
            SELECT pg_notify('commons', 'w:' || $1::text)
        )
        SELECT e.seq FROM e, notify ORDER BY e.seq
        "#,
    )
    .bind(workspace_id)
    .bind(n)
    .bind(&kinds)
    .bind(&audiences)
    .bind(&convs)
    .bind(&msgs)
    .bind(&subjects)
    .bind(&intervals)
    .bind(&payloads)
    .fetch_all(&mut *conn)
    .await?;
    if rows.len() != events.len() {
        return Err(ApiError::NotFound);
    }
    Ok(rows.iter().map(|r| r.get::<i64, _>(0)).collect())
}

#[derive(Debug, Deserialize)]
pub struct SinceQuery {
    pub since: Option<i64>,
    pub limit: Option<i64>,
}

/// The role of `user` in `workspace` if they are a current member.
pub async fn active_role(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
) -> ApiResult<Option<proto::model::Role>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT role FROM workspace_members
         WHERE workspace_id = $1 AND user_id = $2 AND left_seq IS NULL",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.and_then(|r| proto::model::Role::parse(&r.0)))
}

/// Reads one page of a workspace stream for `user`, filtered as of each event's seq.
///
/// The filter fails toward deny: a conversation event is delivered only when its seq lies in
/// one of the user's access intervals and the user still has an open interval now. Events
/// about the user's own membership are always delivered. `next_since` always advances to the
/// end of the scanned range, even when every event in it was filtered out.
pub async fn read_workspace_page(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
    since: Option<i64>,
    limit: i64,
) -> ApiResult<WorkspaceReplay> {
    if active_role(conn, workspace_id, user_id).await?.is_none() {
        return Err(ApiError::Forbidden);
    }
    let (head, floor): (i64, i64) =
        sqlx::query_as("SELECT seq, log_floor FROM workspaces WHERE id = $1")
            .bind(workspace_id)
            .fetch_one(&mut *conn)
            .await?;
    let since = since.unwrap_or(0);
    if since < floor || since > head {
        return crate::sync::snapshot_needed(conn, workspace_id, user_id, head).await;
    }
    let upto = head.min(since + limit.clamp(1, MAX_PAGE));
    let rows = sqlx::query(
        r#"
        SELECT e.seq, e.payload FROM events e
        WHERE e.workspace_id = $1 AND e.seq > $2 AND e.seq <= $3
          AND (
               e.audience = 0
            OR e.subject_user_id = $4
            OR (e.audience = 1
                AND EXISTS (SELECT 1 FROM access_intervals ai
                             WHERE ai.conversation_id = e.conversation_id AND ai.user_id = $4
                               AND ai.joined_seq <= e.seq
                               AND (ai.left_seq IS NULL OR e.seq < ai.left_seq))
                AND EXISTS (SELECT 1 FROM access_intervals ai
                             WHERE ai.conversation_id = e.conversation_id AND ai.user_id = $4
                               AND ai.left_seq IS NULL AND ai.joined_seq IS NOT NULL))
          )
        ORDER BY e.seq
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .bind(upto)
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut events = Vec::with_capacity(rows.len());
    for r in rows {
        let seq: i64 = r.get(0);
        let mut body: WorkspaceEventBody = serde_json::from_value(r.get(1))
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("event {seq}: {e}")))?;
        body.stamp(seq);
        events.push(WorkspaceEvent { seq, body });
    }
    Ok(WorkspaceReplay {
        events,
        next_since: upto,
        head,
        snapshot: None,
    })
}

/// The cursor is outside the retained log. Step 2 replaces this with a snapshot.
pub async fn snapshot_needed(
    _conn: &mut PgConnection,
    _workspace_id: Uuid,
    _user_id: Uuid,
    _head: i64,
) -> ApiResult<WorkspaceReplay> {
    Err(ApiError::Gone(serde_json::json!({
        "error": "resync", "message": "cursor is outside the retained log"
    })))
}

#[utoipa::path(get, path = "/v1/sync/workspaces/{id}",
    params(("id" = Uuid, Path), ("since" = Option<i64>, Query), ("limit" = Option<i64>, Query)),
    responses((status = 200, body = WorkspaceReplay)))]
pub async fn replay_workspace(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
    Query(q): Query<SinceQuery>,
) -> ApiResult<Json<WorkspaceReplay>> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let page = read_workspace_page(
        &mut tx,
        workspace_id,
        auth.user_id,
        q.since,
        q.limit.unwrap_or(MAX_PAGE),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(page))
}

/// Appends a workspace join. Step 2 adds the user-stream event carrying the starting seq.
pub async fn append_join(
    conn: &mut PgConnection,
    workspace: &proto::model::Workspace,
    _user_id: Uuid,
    events: Vec<NewEvent>,
) -> ApiResult<i64> {
    let seqs = append(conn, workspace.id, events).await?;
    Ok(seqs.first().copied().unwrap_or(0))
}

/// Prunes events older than the retention period. Step 2 implements this.
pub async fn prune(_state: &AppState) -> anyhow::Result<()> {
    Ok(())
}
