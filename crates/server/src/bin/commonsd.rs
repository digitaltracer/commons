use std::sync::Arc;

use server::mail::{LogMailer, Mailer, SmtpMailer};
use server::{AppState, Config};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<std::process::ExitCode> {
    if std::env::args().nth(1).as_deref() == Some(server::files::EXTRACT_SUBCOMMAND) {
        return Ok(server::files::extract_pdf_child());
    }
    if std::env::args().nth(1).as_deref() == Some("admin") {
        return admin(std::env::args().skip(2).collect());
    }
    serve()?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// `commonsd admin …`, run on the host with the server's environment.
#[tokio::main(flavor = "current_thread")]
async fn admin(args: Vec<String>) -> anyhow::Result<std::process::ExitCode> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,commons::admin=info".into()),
        )
        .init();
    if args.is_empty() || args[0] == "help" || args[0] == "--help" {
        println!("{}", server::admin::USAGE);
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let config = Config::from_env()?;
    let pool = server::db::connect(&config.database_url).await?;
    server::db::migrate(&pool).await?;
    match server::admin::run(&pool, &config, &args).await {
        Ok(out) => {
            println!("{out}");
            Ok(std::process::ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("{e}");
            Ok(std::process::ExitCode::FAILURE)
        }
    }
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
