//! Integration-test support: one testcontainers Postgres per test process, a fresh database
//! per test, and an in-process server on an ephemeral port.
use crate::sqlx;
use std::sync::{Arc, OnceLock};

use testcontainers::ImageExt;
use testcontainers::core::Mount;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

use crate::mail::MemoryMailer;
use crate::{AppState, Config};

struct Shared {
    admin_url: String,
    port: u16,
}

static PG: OnceLock<Shared> = OnceLock::new();

fn shared() -> &'static Shared {
    PG.get_or_init(|| {
        std::thread::spawn(|| {
            let rt = tokio::runtime::Runtime::new().expect("runtime");
            rt.block_on(async {
                let container = Postgres::default()
                    .with_tag("16-alpine")
                    .with_cmd(["postgres", "-c", "max_connections=500"])
                    // tmpfs keeps test data off the Docker VM disk; commit fsync is near-free.
                    .with_mount(Mount::tmpfs_mount("/var/lib/postgresql/data"))
                    .start()
                    .await
                    .expect("start postgres container (is Docker running?)");
                let port = container.get_host_port_ipv4(5432).await.expect("port");
                let id = container.id().to_string();
                // The container outlives this runtime; a watchdog removes it when the test
                // process exits, however it exits.
                std::mem::forget(container);
                let pid = std::process::id();
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!(
                        "while kill -0 {pid} 2>/dev/null; do sleep 1; done; docker rm -f {id} >/dev/null 2>&1"
                    ))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                Shared {
                    admin_url: format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"),
                    port,
                }
            })
        })
        .join()
        .expect("container thread")
    })
}

/// Creates an empty, migrated database and returns its URL.
pub async fn fresh_database() -> String {
    let s = shared();
    let name = format!("t_{}", Uuid::now_v7().simple());
    let admin = sqlx::PgPool::connect(&s.admin_url)
        .await
        .expect("admin pool");
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .expect("create database");
    admin.close().await;
    let url = format!("postgres://postgres:postgres@127.0.0.1:{}/{name}", s.port);
    let pool = crate::db::connect(&url).await.expect("pool");
    crate::db::migrate(&pool).await.expect("migrate");
    pool.close().await;
    url
}

pub struct TestServer {
    pub base_url: String,
    pub ws_url: String,
    pub state: AppState,
    pub mailer: Arc<MemoryMailer>,
    cancel: tokio_util::sync::CancellationToken,
}

impl TestServer {
    pub fn pool(&self) -> &sqlx::PgPool {
        &self.state.pool
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub fn test_config(database_url: String) -> Config {
    Config {
        database_url,
        auth_attempts_per_minute: 10_000,
        repair_interval: std::time::Duration::from_millis(500),
        job_poll: std::time::Duration::from_millis(50),
        // Test binaries live in target/<profile>/deps; commonsd is one level up.
        extract_program: std::env::current_exe()
            .ok()
            .and_then(|p| Some(p.parent()?.parent()?.join("commonsd"))),
        files_dir: std::env::temp_dir()
            .join(format!("commons-files-{}", Uuid::now_v7().simple()))
            .to_string_lossy()
            .into_owned(),
        ..Config::default()
    }
}

/// Starts a server on a fresh database. `tweak` adjusts the config before start.
pub async fn spawn(tweak: impl FnOnce(&mut Config)) -> TestServer {
    let mut config = test_config(fresh_database().await);
    tweak(&mut config);
    spawn_with(config).await
}

pub async fn spawn_with(config: Config) -> TestServer {
    // Server logs for a test run, filtered by COMMONS_TEST_LOG (for example "warn").
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_env("COMMONS_TEST_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    }
    let mailer = Arc::new(MemoryMailer::default());
    let state = AppState::new(config, mailer.clone()).await.expect("state");
    let cancel = crate::spawn_background(&state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = crate::router(state.clone());
    let stop = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await;
    });
    TestServer {
        base_url: format!("http://{addr}"),
        ws_url: format!("ws://{addr}/v1/socket"),
        state,
        mailer,
        cancel,
    }
}
