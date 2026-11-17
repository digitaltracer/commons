use std::time::Duration;

/// Server settings, read from `COMMONS_*` environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind: String,
    pub public_url: String,
    pub smtp_url: Option<String>,
    pub mail_from: String,
    pub min_client_version: String,
    /// Bounded queue per socket; a full queue disconnects the client.
    pub connection_queue: usize,
    /// A socket write that takes longer than this disconnects the client.
    pub send_timeout: Duration,
    /// Sockets re-read their streams on this period, repairing missed wakeups.
    pub repair_interval: Duration,
    pub auth_attempts_per_minute: u32,
    pub log_retention: Duration,
    pub idempotency_retention: Duration,
    pub job_workers: usize,
    pub job_lease: Duration,
    pub job_poll: Duration,
    pub files_dir: String,
    pub max_file_bytes: u64,
    pub user_quota_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            database_url: "postgres://commons:commons@127.0.0.1:5544/commons".into(),
            bind: "127.0.0.1:8080".into(),
            public_url: "http://127.0.0.1:8080".into(),
            smtp_url: None,
            mail_from: "Commons <no-reply@localhost>".into(),
            min_client_version: "0.1.0".into(),
            connection_queue: 256,
            send_timeout: Duration::from_secs(10),
            repair_interval: Duration::from_secs(15),
            auth_attempts_per_minute: 10,
            log_retention: Duration::from_secs(30 * 24 * 3600),
            idempotency_retention: Duration::from_secs(14 * 24 * 3600),
            job_workers: 4,
            job_lease: Duration::from_secs(60),
            job_poll: Duration::from_millis(500),
            files_dir: "./data/files".into(),
            max_file_bytes: 2 << 30,
            user_quota_bytes: 20 << 30,
        }
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let mut c = Config::default();
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = var("COMMONS_DATABASE_URL") {
            c.database_url = v;
        }
        if let Some(v) = var("COMMONS_BIND") {
            c.bind = v;
        }
        if let Some(v) = var("COMMONS_PUBLIC_URL") {
            c.public_url = v;
        }
        c.smtp_url = var("COMMONS_SMTP_URL");
        if let Some(v) = var("COMMONS_MAIL_FROM") {
            c.mail_from = v;
        }
        if let Some(v) = var("COMMONS_FILES_DIR") {
            c.files_dir = v;
        }
        if let Some(v) = var("COMMONS_MAX_FILE_BYTES") {
            c.max_file_bytes = v.parse()?;
        }
        if let Some(v) = var("COMMONS_USER_QUOTA_BYTES") {
            c.user_quota_bytes = v.parse()?;
        }
        Ok(c)
    }
}
