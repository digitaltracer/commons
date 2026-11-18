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
        crate::oidc::start,
        crate::oidc::callback,
        crate::oidc::exchange,
        crate::oidc::identities,
        crate::oidc::unlink,
        crate::directory::list,
        crate::directory::create,
        crate::directory::members,
        crate::directory::admin_members,
        crate::directory::add_member,
        crate::directory::set_role,
        crate::directory::remove_member,
        crate::invites::create,
        crate::invites::list,
        crate::invites::revoke,
        crate::invites::accept,
        crate::conversations::list_mine,
        crate::conversations::create_channel,
        crate::conversations::browse,
        crate::conversations::join,
        crate::conversations::leave,
        crate::conversations::list_members,
        crate::conversations::add_member,
        crate::conversations::remove_member,
        crate::conversations::open_dm,
        crate::messages::send,
        crate::messages::history,
        crate::files::upload,
        crate::files::download,
        crate::files::download_thumbnail,
        crate::search::handle,
        crate::sync::replay_workspace,
        crate::sync::replay_user,
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
