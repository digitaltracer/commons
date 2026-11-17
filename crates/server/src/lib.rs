//! `commonsd` as a library: one process running HTTP, the WebSocket and bounded workers.
//!
//! Modules follow decision 7: auth, directory, conversations, messages, sync, files, search
//! and notify, plus the hub (real-time fan-out) and jobs (background work).

pub mod auth;
pub mod config;
pub mod conversations;
pub mod db;
pub mod directory;
pub mod error;
pub mod hub;
pub mod idempotency;
pub mod jobs;
pub mod mail;
pub mod messages;
pub mod openapi;
pub mod sqlx;
pub mod sync;
pub mod ws;

#[cfg(feature = "testing")]
pub mod testing;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

pub use config::Config;

pub struct Inner {
    pub pool: sqlx::PgPool,
    pub config: Config,
    pub hub: hub::Hub,
    pub mailer: Arc<dyn mail::Mailer>,
    pub limiter: auth::Limiter,
}

/// Shared server state. Cheap to clone.
#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub async fn new(config: Config, mailer: Arc<dyn mail::Mailer>) -> anyhow::Result<Self> {
        let pool = db::connect(&config.database_url).await?;
        db::migrate(&pool).await?;
        Ok(Self::with_pool(config, pool, mailer))
    }

    pub fn with_pool(config: Config, pool: sqlx::PgPool, mailer: Arc<dyn mail::Mailer>) -> Self {
        let limiter = auth::Limiter::new(config.auth_attempts_per_minute);
        let hub = hub::Hub::new(config.connection_queue);
        AppState(Arc::new(Inner {
            pool,
            config,
            hub,
            mailer,
            limiter,
        }))
    }
}

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/meta", get(meta))
        .route("/openapi.json", get(openapi::serve))
        .merge(auth::routes())
        .merge(directory::routes())
        .merge(conversations::routes())
        .merge(messages::routes())
        .merge(sync::routes())
        .route("/socket", get(ws::upgrade))
        .route("/health", post(health).get(health));
    Router::new()
        .nest("/v1", v1)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Starts the background tasks: the NOTIFY listener and the job workers.
pub fn spawn_background(state: &AppState) -> tokio_util::sync::CancellationToken {
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(hub::listen(state.clone(), cancel.clone()));
    tokio::spawn(jobs::run_workers(state.clone(), cancel.clone()));
    cancel
}

#[utoipa::path(get, path = "/v1/meta", responses((status = 200, body = proto::api::Meta)))]
pub async fn meta(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<proto::api::Meta> {
    axum::Json(proto::api::Meta {
        protocol_version: proto::PROTOCOL_VERSION,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        min_client_version: state.config.min_client_version.clone(),
    })
}

async fn health() -> &'static str {
    "ok"
}
