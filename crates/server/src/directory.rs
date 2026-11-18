//! Workspaces, workspace members and roles.
use crate::sqlx;
use crate::sqlx::PgConnection;
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use proto::api::{
    AddWorkspaceMember, AdminMember, AdminMemberList, CreateWorkspace, MemberList, SetRole,
    WorkspaceList,
};
use proto::events::UserEventBody;
use proto::events::{AccessVia, WorkspaceEventBody};
use proto::model::{ConversationKind, Member, Role, Workspace};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::conversations;
use crate::error::{ApiError, ApiResult};
use crate::sync::{self, NewEvent};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/workspaces", get(list).post(create))
        .route("/workspaces/{id}/members", get(members).post(add_member))
        .route("/workspaces/{id}/admin/members", get(admin_members))
        .route(
            "/workspaces/{id}/members/{user_id}",
            axum::routing::patch(set_role).delete(remove_member),
        )
}

pub async fn require_role(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
    allowed: &[Role],
) -> ApiResult<Role> {
    match sync::active_role(conn, workspace_id, user_id).await? {
        Some(r) if allowed.contains(&r) => Ok(r),
        Some(_) => Err(ApiError::Forbidden),
        None => Err(ApiError::Forbidden),
    }
}

async fn user_name(conn: &mut PgConnection, user_id: Uuid) -> ApiResult<String> {
    Ok(
        sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// Adds `user` to a workspace: the membership row, the default channels, and the
/// role-derived access to private channels. Returns the workspace events to append.
pub async fn join(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
    role: Role,
) -> ApiResult<Vec<NewEvent>> {
    let display_name = user_name(conn, user_id).await?;
    let inserted = sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role, display_name, joined_seq)
         VALUES ($1, $2, $3, $4, 0)
         ON CONFLICT (workspace_id, user_id) DO UPDATE
            SET role = EXCLUDED.role, display_name = EXCLUDED.display_name,
                joined_seq = 0, left_seq = NULL, joined_at = now()
          WHERE workspace_members.left_seq IS NOT NULL",
    )
    .bind(workspace_id)
    .bind(user_id)
    .bind(role.as_str())
    .bind(&display_name)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(ApiError::Conflict(serde_json::json!({
            "error": "already_member", "message": "already a member of this workspace"
        })));
    }
    let mut events = vec![
        NewEvent::workspace(WorkspaceEventBody::WorkspaceMemberJoined {
            member: Member {
                user_id,
                workspace_id,
                display_name,
                role,
            },
        })
        .about(user_id),
    ];
    let defaults: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE workspace_id = $1 AND is_default ORDER BY created_at",
    )
    .bind(workspace_id)
    .fetch_all(&mut *conn)
    .await?;
    if role != Role::Guest {
        for id in defaults {
            let c = conversations::load(conn, id).await?;
            events.extend(conversations::grant(conn, &c, user_id, AccessVia::Membership).await?);
        }
    }
    if role.reads_private_channels() {
        events.extend(grant_role_access(conn, workspace_id, user_id).await?);
    }
    Ok(events)
}

/// Opens role-derived intervals on every private channel in the workspace.
pub async fn grant_role_access(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
) -> ApiResult<Vec<NewEvent>> {
    let private: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE workspace_id = $1 AND kind = 'private'",
    )
    .bind(workspace_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut events = vec![];
    for id in private {
        let c = conversations::load(conn, id).await?;
        events.extend(conversations::grant(conn, &c, user_id, AccessVia::Role).await?);
    }
    Ok(events)
}

#[utoipa::path(get, path = "/v1/workspaces", responses((status = 200, body = WorkspaceList)))]
pub async fn list(State(state): State<AppState>, auth: AuthUser) -> ApiResult<Json<WorkspaceList>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT w.id, w.name FROM workspaces w JOIN workspace_members m ON m.workspace_id = w.id
          WHERE m.user_id = $1 AND m.left_seq IS NULL ORDER BY w.created_at",
    )
    .bind(auth.user_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(WorkspaceList {
        workspaces: rows
            .into_iter()
            .map(|(id, name)| Workspace { id, name })
            .collect(),
    }))
}

/// Creates a workspace with a default `#general` channel; the caller becomes its owner.
#[utoipa::path(post, path = "/v1/workspaces", request_body = CreateWorkspace,
    responses((status = 200, body = Workspace)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateWorkspace>,
) -> ApiResult<Json<Workspace>> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err(ApiError::BadRequest("workspace name is required".into()));
    }
    let id = Uuid::now_v7();
    let mut tx = state.pool.begin().await?;
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    let general = conversations::create(
        &mut tx,
        id,
        ConversationKind::Public,
        Some("general"),
        None,
        true,
    )
    .await?;
    let mut events = vec![NewEvent::workspace(
        WorkspaceEventBody::ConversationCreated {
            conversation: general,
        },
    )];
    events.extend(join(&mut tx, id, auth.user_id, Role::Owner).await?);
    let workspace = Workspace {
        id,
        name: name.to_string(),
    };
    crate::sync::append_join(&mut tx, &workspace, auth.user_id, events).await?;
    tx.commit().await?;
    Ok(Json(workspace))
}

#[utoipa::path(get, path = "/v1/workspaces/{id}/members", params(("id" = Uuid, Path)),
    responses((status = 200, body = MemberList)))]
pub async fn members(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<MemberList>> {
    let mut conn = state.pool.acquire().await?;
    sync::active_role(&mut conn, workspace_id, auth.user_id)
        .await?
        .ok_or(ApiError::Forbidden)?;
    Ok(Json(MemberList {
        members: list_members(&mut conn, workspace_id).await?,
    }))
}

pub async fn list_members(conn: &mut PgConnection, workspace_id: Uuid) -> ApiResult<Vec<Member>> {
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT user_id, display_name, role FROM workspace_members
          WHERE workspace_id = $1 AND left_seq IS NULL ORDER BY display_name",
    )
    .bind(workspace_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(user_id, display_name, role)| {
            Some(Member {
                user_id,
                workspace_id,
                display_name,
                role: Role::parse(&role)?,
            })
        })
        .collect())
}

/// Adds an existing account to the workspace. Owners and admins only.
#[utoipa::path(post, path = "/v1/workspaces/{id}/members", params(("id" = Uuid, Path)),
    request_body = AddWorkspaceMember, responses((status = 200, body = Member)))]
pub async fn add_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<AddWorkspaceMember>,
) -> ApiResult<Json<Member>> {
    let mut tx = state.pool.begin().await?;
    sync::lock_workspace(&mut tx, workspace_id).await?;
    let mine = require_role(
        &mut tx,
        workspace_id,
        auth.user_id,
        &[Role::Owner, Role::Admin],
    )
    .await?;
    if req.role == Role::Owner && mine != Role::Owner {
        return Err(ApiError::Forbidden);
    }
    let email = req.email.trim().to_lowercase();
    let (user_id, display_name): (Uuid, String) =
        sqlx::query_as("SELECT id, display_name FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::NotFound)?;
    let name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id = $1")
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await?;
    let events = join(&mut tx, workspace_id, user_id, req.role).await?;
    crate::sync::append_join(
        &mut tx,
        &Workspace {
            id: workspace_id,
            name,
        },
        user_id,
        events,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(Member {
        user_id,
        workspace_id,
        display_name,
        role: req.role,
    }))
}

/// Changes a member's role. Only an owner grants or removes owner, and the last owner stays.
#[utoipa::path(patch, path = "/v1/workspaces/{id}/members/{user_id}",
    params(("id" = Uuid, Path), ("user_id" = Uuid, Path)), request_body = SetRole,
    responses((status = 204), (status = 403)))]
pub async fn set_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((workspace_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetRole>,
) -> ApiResult<axum::http::StatusCode> {
    let mut tx = state.pool.begin().await?;
    sync::lock_workspace(&mut tx, workspace_id).await?;
    let mine = require_role(
        &mut tx,
        workspace_id,
        auth.user_id,
        &[Role::Owner, Role::Admin],
    )
    .await?;
    let old = sync::active_role(&mut tx, workspace_id, user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if (old == Role::Owner || req.role == Role::Owner) && mine != Role::Owner {
        return Err(ApiError::Forbidden);
    }
    change_role(&mut tx, workspace_id, user_id, old, req.role).await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Refuses to leave the workspace without an owner once `user_id` stops being one.
async fn keep_an_owner(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
) -> ApiResult<()> {
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_members
          WHERE workspace_id = $1 AND user_id <> $2 AND left_seq IS NULL AND role = 'owner'",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await?;
    if others == 0 {
        return Err(ApiError::Refused(
            "last_owner",
            "a workspace needs an owner: make someone else an owner first".into(),
        ));
    }
    Ok(())
}

/// Sets a member's role from `old` to `new`. Gaining or losing admin opens or closes
/// role-derived access to every private channel in the same transaction. The caller holds the
/// workspace lock and has checked that the actor may do this.
pub async fn change_role(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
    old: Role,
    new: Role,
) -> ApiResult<()> {
    if old == new {
        return Ok(());
    }
    if old == Role::Owner {
        keep_an_owner(conn, workspace_id, user_id).await?;
    }
    sqlx::query("UPDATE workspace_members SET role = $3 WHERE workspace_id = $1 AND user_id = $2")
        .bind(workspace_id)
        .bind(user_id)
        .bind(new.as_str())
        .execute(&mut *conn)
        .await?;
    let mut events = vec![NewEvent::workspace(WorkspaceEventBody::MemberRoleChanged {
        user_id,
        role: new,
    })];
    match (old.reads_private_channels(), new.reads_private_channels()) {
        (false, true) => events.extend(grant_role_access(conn, workspace_id, user_id).await?),
        (true, false) => {
            let private: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM conversations WHERE workspace_id = $1 AND kind = 'private'",
            )
            .bind(workspace_id)
            .fetch_all(&mut *conn)
            .await?;
            for c in private {
                events.extend(conversations::revoke(conn, c, user_id, AccessVia::Role).await?);
            }
        }
        _ => {}
    }
    sync::append(conn, workspace_id, events).await?;
    Ok(())
}

/// Members with their join dates and whether they are connected, for owners and admins.
#[utoipa::path(get, path = "/v1/workspaces/{id}/admin/members", params(("id" = Uuid, Path)),
    responses((status = 200, body = AdminMemberList), (status = 403)))]
pub async fn admin_members(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<AdminMemberList>> {
    let mut conn = state.pool.acquire().await?;
    require_role(
        &mut conn,
        workspace_id,
        auth.user_id,
        &[Role::Owner, Role::Admin],
    )
    .await?;
    let rows: Vec<(Uuid, String, String, time::OffsetDateTime)> = sqlx::query_as(
        "SELECT user_id, display_name, role, joined_at FROM workspace_members
          WHERE workspace_id = $1 AND left_seq IS NULL ORDER BY display_name",
    )
    .bind(workspace_id)
    .fetch_all(&mut *conn)
    .await?;
    let online: std::collections::HashSet<Uuid> =
        state.hub.online_in(workspace_id).into_iter().collect();
    Ok(Json(AdminMemberList {
        members: rows
            .into_iter()
            .filter_map(|(user_id, display_name, role, joined)| {
                Some(AdminMember {
                    user_id,
                    display_name,
                    role: Role::parse(&role)?,
                    joined_at_ms: crate::db::ms(joined),
                    online: online.contains(&user_id),
                })
            })
            .collect(),
    }))
}

/// Removes someone from the workspace: every access interval closes, and the user's own
/// stream gets `workspace_removed`, which drives their client's purge.
#[utoipa::path(delete, path = "/v1/workspaces/{id}/members/{user_id}",
    params(("id" = Uuid, Path), ("user_id" = Uuid, Path)), responses((status = 204), (status = 403)))]
pub async fn remove_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((workspace_id, user_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::http::StatusCode> {
    let mut tx = state.pool.begin().await?;
    sync::lock_workspace(&mut tx, workspace_id).await?;
    let mine = sync::active_role(&mut tx, workspace_id, auth.user_id)
        .await?
        .ok_or(ApiError::Forbidden)?;
    let theirs = sync::active_role(&mut tx, workspace_id, user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let allowed = user_id == auth.user_id
        || mine == Role::Owner
        || (mine == Role::Admin && theirs != Role::Owner);
    if !allowed {
        return Err(ApiError::Forbidden);
    }
    if theirs == Role::Owner {
        keep_an_owner(&mut tx, workspace_id, user_id).await?;
    }
    let mut events = conversations::revoke_all(&mut tx, workspace_id, user_id).await?;
    events.push(
        NewEvent::workspace(WorkspaceEventBody::WorkspaceMemberLeft { user_id }).about(user_id),
    );
    sync::append(&mut tx, workspace_id, events).await?;
    sync::append_user(
        &mut tx,
        user_id,
        vec![UserEventBody::WorkspaceRemoved { workspace_id }],
    )
    .await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
