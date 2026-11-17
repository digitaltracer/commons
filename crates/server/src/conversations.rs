//! Conversations and access intervals (decision 10).
use crate::sqlx;
use crate::sqlx::{PgConnection, Row};
use axum::extract::{Path, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use proto::api::{AddConversationMember, ConversationList, CreateChannel, MemberList, OpenDm};
use proto::events::{AccessVia, WorkspaceEventBody};
use proto::model::{Conversation, ConversationKind, Member, Role};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::{ApiError, ApiResult};
use crate::sync::{self, NewEvent};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/workspaces/{id}/conversations", get(list_mine))
        .route(
            "/workspaces/{id}/channels",
            get(browse).post(create_channel),
        )
        .route("/workspaces/{id}/dms", post(open_dm))
        .route("/conversations/{id}/join", post(join))
        .route("/conversations/{id}/leave", post(leave))
        .route(
            "/conversations/{id}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/conversations/{id}/members/{user_id}",
            delete(remove_member),
        )
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
/// Every open interval for the pair is locked first, so two concurrent revocations agree.
pub async fn revoke(
    conn: &mut PgConnection,
    conversation_id: Uuid,
    user_id: Uuid,
    via: AccessVia,
) -> ApiResult<Option<NewEvent>> {
    let open: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, via FROM access_intervals WHERE conversation_id = $1 AND user_id = $2
           AND left_seq IS NULL ORDER BY id FOR UPDATE",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    let via_s = via_str(via);
    let Some((id, _)) = open.iter().find(|(_, v)| v == via_s) else {
        return Ok(None);
    };
    let has_access = open.iter().any(|(_, v)| v != via_s);
    Ok(Some(
        NewEvent::conversation(WorkspaceEventBody::MemberRemoved {
            conversation_id,
            user_id,
            via,
            has_access,
        })
        .about(user_id)
        .interval(*id),
    ))
}

/// Closes every interval `user` holds in a workspace (removal from the workspace).
pub async fn revoke_all(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
) -> ApiResult<Vec<NewEvent>> {
    let open: Vec<(i64, Uuid, String)> = sqlx::query_as(
        "SELECT ai.id, ai.conversation_id, ai.via FROM access_intervals ai
           JOIN conversations c ON c.id = ai.conversation_id
          WHERE c.workspace_id = $1 AND ai.user_id = $2 AND ai.left_seq IS NULL
          ORDER BY ai.id FOR UPDATE OF ai",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(open
        .into_iter()
        .map(|(id, conversation_id, via)| {
            let via = if via == "role" {
                AccessVia::Role
            } else {
                AccessVia::Membership
            };
            NewEvent::conversation(WorkspaceEventBody::MemberRemoved {
                conversation_id,
                user_id,
                via,
                has_access: false,
            })
            .about(user_id)
            .interval(id)
        })
        .collect())
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

async fn workspace_of(conn: &mut PgConnection, conversation_id: Uuid) -> ApiResult<Uuid> {
    sqlx::query_scalar("SELECT workspace_id FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(ApiError::NotFound)
}

async fn role_in(conn: &mut PgConnection, workspace_id: Uuid, user_id: Uuid) -> ApiResult<Role> {
    sync::active_role(conn, workspace_id, user_id)
        .await?
        .ok_or(ApiError::Forbidden)
}

fn check_channel_name(name: &str) -> ApiResult<String> {
    let n = name.trim().trim_start_matches('#').to_lowercase();
    let ok = !n.is_empty()
        && n.len() <= 80
        && n.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(ApiError::BadRequest(
            "channel names use letters, digits, - and _".into(),
        ));
    }
    Ok(n)
}

/// Creates a public or private channel; the creator joins it. Owners and admins get
/// role-derived access to a private channel in the same transaction.
#[utoipa::path(post, path = "/v1/workspaces/{id}/channels", params(("id" = Uuid, Path)),
    request_body = CreateChannel, responses((status = 200, body = Conversation)))]
pub async fn create_channel(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<CreateChannel>,
) -> ApiResult<Json<Conversation>> {
    if !matches!(
        req.kind,
        ConversationKind::Public | ConversationKind::Private
    ) {
        return Err(ApiError::BadRequest("use the DM endpoint for DMs".into()));
    }
    let name = check_channel_name(&req.name)?;
    let mut tx = state.pool.begin().await?;
    if role_in(&mut tx, workspace_id, auth.user_id).await? == Role::Guest {
        return Err(ApiError::Forbidden);
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversations WHERE workspace_id = $1 AND name = $2)",
    )
    .bind(workspace_id)
    .bind(&name)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(ApiError::Conflict(serde_json::json!({
            "error": "name_taken", "message": "a channel with this name exists"
        })));
    }
    let c = create(&mut tx, workspace_id, req.kind, Some(&name), None, false).await?;
    let mut events = vec![];
    if c.kind == ConversationKind::Public {
        events.push(NewEvent::workspace(
            WorkspaceEventBody::ConversationCreated {
                conversation: c.clone(),
            },
        ));
    }
    events.extend(grant(&mut tx, &c, auth.user_id, AccessVia::Membership).await?);
    if c.kind == ConversationKind::Private {
        let admins: Vec<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM workspace_members WHERE workspace_id = $1
               AND left_seq IS NULL AND role IN ('owner', 'admin')",
        )
        .bind(workspace_id)
        .fetch_all(&mut *tx)
        .await?;
        for a in admins {
            events.extend(grant(&mut tx, &c, a, AccessVia::Role).await?);
        }
    }
    sync::append(&mut tx, workspace_id, events).await?;
    tx.commit().await?;
    Ok(Json(c))
}

/// Channels the caller can see: every public channel (except for guests) and the private
/// channels they can read.
#[utoipa::path(get, path = "/v1/workspaces/{id}/channels", params(("id" = Uuid, Path)),
    responses((status = 200, body = ConversationList)))]
pub async fn browse(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<ConversationList>> {
    let mut conn = state.pool.acquire().await?;
    let role = role_in(&mut conn, workspace_id, auth.user_id).await?;
    let sql = format!(
        "SELECT {CONVERSATION_COLUMNS} FROM conversations c
          WHERE c.workspace_id = $1 AND c.kind IN ('public', 'private') AND (
              (c.kind = 'public' AND $3)
              OR EXISTS (SELECT 1 FROM access_intervals ai WHERE ai.conversation_id = c.id
                          AND ai.user_id = $2 AND ai.left_seq IS NULL))
          ORDER BY c.name"
    );
    let rows = sqlx::query(&sql)
        .bind(workspace_id)
        .bind(auth.user_id)
        .bind(role != Role::Guest)
        .fetch_all(&mut *conn)
        .await?;
    Ok(Json(ConversationList {
        conversations: rows
            .iter()
            .map(row_to_conversation)
            .collect::<ApiResult<_>>()?,
    }))
}

#[utoipa::path(post, path = "/v1/conversations/{id}/join", params(("id" = Uuid, Path)),
    responses((status = 204), (status = 403)))]
pub async fn join(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    let mut tx = state.pool.begin().await?;
    let c = load(&mut tx, conversation_id).await?;
    let role = role_in(&mut tx, c.workspace_id, auth.user_id).await?;
    if c.kind != ConversationKind::Public || role == Role::Guest {
        return Err(ApiError::Forbidden);
    }
    if let Some(e) = grant(&mut tx, &c, auth.user_id, AccessVia::Membership).await? {
        sync::append(&mut tx, c.workspace_id, vec![e]).await?;
    }
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/v1/conversations/{id}/leave", params(("id" = Uuid, Path)),
    responses((status = 204)))]
pub async fn leave(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    let mut tx = state.pool.begin().await?;
    let ws = workspace_of(&mut tx, conversation_id).await?;
    if let Some(e) = revoke(
        &mut tx,
        conversation_id,
        auth.user_id,
        AccessVia::Membership,
    )
    .await?
    {
        sync::append(&mut tx, ws, vec![e]).await?;
        rekey_group_dm(&mut tx, conversation_id).await?;
    }
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Current members of a conversation: open membership intervals of current workspace
/// members, with their workspace profile. Only someone who can read it now may list them.
#[utoipa::path(get, path = "/v1/conversations/{id}/members", params(("id" = Uuid, Path)),
    responses((status = 200, body = MemberList), (status = 403)))]
pub async fn list_members(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
) -> ApiResult<Json<MemberList>> {
    let mut conn = state.pool.acquire().await?;
    if !can_read_now(&mut conn, conversation_id, auth.user_id).await {
        return Err(ApiError::Forbidden);
    }
    let rows: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT DISTINCT wm.user_id, wm.workspace_id, wm.display_name, wm.role
           FROM access_intervals ai
           JOIN conversations c ON c.id = ai.conversation_id
           JOIN workspace_members wm ON wm.workspace_id = c.workspace_id
                AND wm.user_id = ai.user_id AND wm.left_seq IS NULL
          WHERE ai.conversation_id = $1 AND ai.via = 'membership' AND ai.left_seq IS NULL
          ORDER BY wm.display_name, wm.user_id",
    )
    .bind(conversation_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(MemberList {
        members: rows
            .into_iter()
            .filter_map(|(user_id, workspace_id, display_name, role)| {
                Some(Member {
                    user_id,
                    workspace_id,
                    display_name,
                    role: Role::parse(&role)?,
                })
            })
            .collect(),
    }))
}

/// Keeps a group DM's key equal to its current members, so opening a DM with exactly these
/// people finds it. If another group DM already has that set, this one keeps no key.
async fn rekey_group_dm(conn: &mut PgConnection, conversation_id: Uuid) -> ApiResult<()> {
    sqlx::query(
        "UPDATE conversations c SET dm_key = CASE WHEN EXISTS (
                SELECT 1 FROM conversations o WHERE o.workspace_id = c.workspace_id
                   AND o.id <> c.id AND o.dm_key = k.key) THEN NULL ELSE k.key END
           FROM (SELECT string_agg(DISTINCT user_id::text, ',' ORDER BY user_id::text) AS key
                   FROM access_intervals
                  WHERE conversation_id = $1 AND via = 'membership' AND left_seq IS NULL) k
          WHERE c.id = $1 AND c.kind = 'group_dm'",
    )
    .bind(conversation_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Adds a workspace member to a channel or group DM. Anyone who can read it may add people.
#[utoipa::path(post, path = "/v1/conversations/{id}/members", params(("id" = Uuid, Path)),
    request_body = AddConversationMember, responses((status = 204), (status = 403)))]
pub async fn add_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
    Json(req): Json<AddConversationMember>,
) -> ApiResult<axum::http::StatusCode> {
    let mut tx = state.pool.begin().await?;
    let c = load(&mut tx, conversation_id).await?;
    if c.kind == ConversationKind::Dm || !can_read_now(&mut tx, c.id, auth.user_id).await {
        return Err(ApiError::Forbidden);
    }
    role_in(&mut tx, c.workspace_id, req.user_id)
        .await
        .map_err(|_| ApiError::BadRequest("not a member of this workspace".into()))?;
    if c.kind == ConversationKind::GroupDm {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT user_id) FROM access_intervals
              WHERE conversation_id = $1 AND via = 'membership' AND left_seq IS NULL",
        )
        .bind(c.id)
        .fetch_one(&mut *tx)
        .await?;
        if count >= 9 {
            return Err(ApiError::BadRequest(
                "group DMs hold at most 9 people".into(),
            ));
        }
    }
    let mut c = c;
    if c.kind == ConversationKind::GroupDm && !c.dm_user_ids.contains(&req.user_id) {
        // The event carries the conversation as it is after the add, so clients retitle it.
        c.dm_user_ids.push(req.user_id);
        c.dm_user_ids.sort();
    }
    if let Some(e) = grant(&mut tx, &c, req.user_id, AccessVia::Membership).await? {
        sync::append(&mut tx, c.workspace_id, vec![e]).await?;
        rekey_group_dm(&mut tx, c.id).await?;
    }
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Removes someone's membership of a channel. Owners and admins, or the member themself.
/// Nobody removes someone else from a DM or group DM; they can only leave.
#[utoipa::path(delete, path = "/v1/conversations/{id}/members/{user_id}",
    params(("id" = Uuid, Path), ("user_id" = Uuid, Path)), responses((status = 204), (status = 403)))]
pub async fn remove_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((conversation_id, user_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::http::StatusCode> {
    let mut tx = state.pool.begin().await?;
    let c = load(&mut tx, conversation_id).await?;
    let role = role_in(&mut tx, c.workspace_id, auth.user_id).await?;
    let is_dm = matches!(c.kind, ConversationKind::Dm | ConversationKind::GroupDm);
    if user_id != auth.user_id && (is_dm || !role.reads_private_channels()) {
        return Err(ApiError::Forbidden);
    }
    if let Some(e) = revoke(&mut tx, c.id, user_id, AccessVia::Membership).await? {
        sync::append(&mut tx, c.workspace_id, vec![e]).await?;
        rekey_group_dm(&mut tx, c.id).await?;
    }
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Opens (or returns) the DM or group DM between the caller and `user_ids`.
#[utoipa::path(post, path = "/v1/workspaces/{id}/dms", params(("id" = Uuid, Path)),
    request_body = OpenDm, responses((status = 200, body = Conversation)))]
pub async fn open_dm(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<OpenDm>,
) -> ApiResult<Json<Conversation>> {
    let mut ids: Vec<Uuid> = req.user_ids.clone();
    ids.push(auth.user_id);
    ids.sort();
    ids.dedup();
    if ids.len() > 9 {
        return Err(ApiError::BadRequest(
            "group DMs hold at most 9 people".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    for u in &ids {
        role_in(&mut tx, workspace_id, *u)
            .await
            .map_err(|_| ApiError::BadRequest("everyone must be in this workspace".into()))?;
    }
    let key = ids
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM conversations WHERE workspace_id = $1 AND dm_key = $2")
            .bind(workspace_id)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(id) = existing {
        tx.rollback().await?;
        let mut conn = state.pool.acquire().await?;
        return Ok(Json(load(&mut conn, id).await?));
    }
    let kind = if ids.len() <= 2 {
        ConversationKind::Dm
    } else {
        ConversationKind::GroupDm
    };
    let mut c = create(&mut tx, workspace_id, kind, None, Some(&key), false).await?;
    c.dm_user_ids = ids.clone();
    let mut events = vec![];
    for u in &ids {
        events.extend(grant(&mut tx, &c, *u, AccessVia::Membership).await?);
    }
    sync::append(&mut tx, workspace_id, events).await?;
    tx.commit().await?;
    Ok(Json(c))
}
