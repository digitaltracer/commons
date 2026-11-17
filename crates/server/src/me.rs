//! Per-user state: read cursors and preferences, both carried on the user stream, and
//! presence as this node sees it.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use proto::api::MarkRead;
use proto::events::UserEventBody;
use proto::model::Preferences;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{ApiError, ApiResult};
use crate::sqlx;
use crate::{AppState, conversations, sync};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/conversations/{id}/read", post(mark_read))
        .route("/me/preferences", put(set_preferences).get(get_preferences))
        .route("/workspaces/{id}/presence", get(presence))
}

/// Moves the read cursor forward (never back) and tells the user's other devices.
#[utoipa::path(post, path = "/v1/conversations/{id}/read", params(("id" = Uuid, Path)),
    request_body = MarkRead, responses((status = 204)))]
pub async fn mark_read(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(conversation_id): Path<Uuid>,
    Json(req): Json<MarkRead>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    if !conversations::can_read_now(&mut tx, conversation_id, auth.user_id).await {
        return Err(ApiError::Forbidden);
    }
    let workspace_id: Uuid =
        sqlx::query_scalar("SELECT workspace_id FROM conversations WHERE id = $1")
            .bind(conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    let moved = sqlx::query(
        "INSERT INTO read_cursors (user_id, conversation_id, workspace_id, seq) VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, conversation_id) DO UPDATE SET seq = EXCLUDED.seq
          WHERE read_cursors.seq < EXCLUDED.seq",
    )
    .bind(auth.user_id)
    .bind(conversation_id)
    .bind(workspace_id)
    .bind(req.seq)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if moved {
        sync::append_user(
            &mut tx,
            auth.user_id,
            vec![UserEventBody::ReadCursor {
                workspace_id,
                conversation_id,
                seq: req.seq,
            }],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/v1/me/preferences", responses((status = 200, body = Preferences)))]
pub async fn get_preferences(
    State(state): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<Preferences>> {
    let v: serde_json::Value = sqlx::query_scalar("SELECT preferences FROM users WHERE id = $1")
        .bind(auth.user_id)
        .fetch_one(&state.pool)
        .await?;
    Ok(Json(serde_json::from_value(v).unwrap_or_default()))
}

#[utoipa::path(put, path = "/v1/me/preferences", request_body = Preferences, responses((status = 204)))]
pub async fn set_preferences(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(prefs): Json<Preferences>,
) -> ApiResult<StatusCode> {
    if prefs.keywords.len() > 50 || prefs.muted.len() > 1000 {
        return Err(ApiError::BadRequest("too many keywords or mutes".into()));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE users SET preferences = $2 WHERE id = $1")
        .bind(auth.user_id)
        .bind(serde_json::to_value(&prefs).map_err(|e| ApiError::Internal(e.into()))?)
        .execute(&mut *tx)
        .await?;
    sync::append_user(
        &mut tx,
        auth.user_id,
        vec![UserEventBody::PreferencesChanged { preferences: prefs }],
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Users connected to this node in the workspace. Presence is ephemeral and never logged.
#[utoipa::path(get, path = "/v1/workspaces/{id}/presence", params(("id" = Uuid, Path)),
    responses((status = 200, body = Vec<Uuid>)))]
pub async fn presence(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<Vec<Uuid>>> {
    let mut conn = state.pool.acquire().await?;
    sync::active_role(&mut conn, workspace_id, auth.user_id)
        .await?
        .ok_or(ApiError::Forbidden)?;
    Ok(Json(state.hub.online_in(workspace_id)))
}
