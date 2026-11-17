//! The OpenAPI document, generated from the handlers, with the event schemas.
use axum::Json;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Commons API",
        version = "1",
        description = "JSON over HTTPS plus one WebSocket (`/v1/socket`) per client."
    ),
    paths(
        crate::meta,
        crate::auth::register,
        crate::auth::login,
        crate::auth::logout,
        crate::auth::me,
        crate::auth::reset_request,
        crate::auth::reset_confirm,
        crate::directory::list,
        crate::directory::create,
        crate::directory::members,
        crate::directory::add_member,
        crate::conversations::list_mine,
        crate::messages::send,
        crate::messages::history,
        crate::sync::replay_workspace,
    ),
    components(schemas(
        proto::events::WorkspaceEvent,
        proto::events::WorkspaceEventBody,
        proto::events::WorkspaceReplay,
        proto::events::WorkspaceSnapshot,
        proto::events::UserEvent,
        proto::events::UserEventBody,
        proto::events::UserReplay,
        proto::events::UserSnapshot,
        proto::api::ErrorBody,
    ))
)]
pub struct ApiDoc;

pub async fn serve() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}
