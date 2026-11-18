use std::sync::Arc;

use server::mail::{LogMailer, Mailer, SmtpMailer};
use server::{AppState, Config};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<std::process::ExitCode> {
    if std::env::args().nth(1).as_deref() == Some(server::files::EXTRACT_SUBCOMMAND) {
        return Ok(server::files::extract_pdf_child());
    }
    serve()?;
    Ok(std::process::ExitCode::SUCCESS)
}

#[tokio::main]
async fn serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = Config::from_env()?;
    let mailer: Arc<dyn Mailer> = match &config.smtp_url {
        Some(url) => Arc::new(SmtpMailer::new(url, &config.mail_from)?),
        None => Arc::new(LogMailer),
    };
    let bind = config.bind.clone();
    let state = AppState::new(config, mailer).await?;
    let cancel = server::spawn_background(&state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "commonsd listening");
    axum::serve(
        listener,
        server::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    })
    .await?;
    Ok(())
}
