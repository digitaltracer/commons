use std::sync::Arc;

use server::mail::{Mailer, MemoryMailer, SmtpMailer};
use server::{AppState, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = Config::from_env()?;
    let mailer: Arc<dyn Mailer> = match &config.smtp_url {
        Some(url) => Arc::new(SmtpMailer::new(url, &config.mail_from)?),
        None => Arc::new(MemoryMailer::default()),
    };
    let bind = config.bind.clone();
    let state = AppState::new(config, mailer).await?;
    let cancel = server::spawn_background(&state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "commonsd listening");
    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        })
        .await?;
    Ok(())
}
