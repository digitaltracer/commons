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
    /// Login and reset attempts per email per minute; five times this per client IP.
    pub auth_attempts_per_minute: u32,
    /// Proxies, besides loopback, whose `X-Real-IP` / `X-Forwarded-For` are believed.
    pub trusted_proxies: Vec<std::net::IpAddr>,
    pub log_retention: Duration,
    pub idempotency_retention: Duration,
    pub job_workers: usize,
    /// Must outlast `extract_timeout`, so a running extraction is never claimed twice.
    pub job_lease: Duration,
    pub job_poll: Duration,
    /// The PDF extractor program, run as `<program> __extract-pdf`; this binary by default.
    pub extract_program: Option<std::path::PathBuf>,
    pub extract_timeout: Duration,
    /// Local file store root, used when `s3_bucket` is unset.
    pub files_dir: String,
    /// S3-compatible bucket; credentials, region and endpoint come from the `AWS_*` variables.
    pub s3_bucket: Option<String>,
    pub max_file_bytes: u64,
    pub user_quota_bytes: u64,
    pub registration: Registration,
    /// Sign-in through an OIDC provider; on only when all four `COMMONS_OIDC_*` are set.
    pub oidc: Option<OidcConfig>,
}

#[derive(Clone)]
pub struct OidcConfig {
    /// For example `https://accounts.google.com`; discovery is read from under it.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// The provider's name on the sign-in button, for example "Google".
    pub name: String,
}

impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl OidcConfig {
    /// All four set: on. None set: off. Some set: off, naming the missing ones.
    pub fn from_vars(
        issuer: Option<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
        name: Option<String>,
    ) -> Result<Option<Self>, Vec<&'static str>> {
        match (issuer, client_id, client_secret, name) {
            (Some(issuer), Some(client_id), Some(client_secret), Some(name)) => {
                Ok(Some(OidcConfig {
                    issuer: issuer.trim().trim_end_matches('/').to_string(),
                    client_id: client_id.trim().to_string(),
                    client_secret: client_secret.trim().to_string(),
                    name: name.trim().to_string(),
                }))
            }
            (None, None, None, None) => Ok(None),
            (i, c, s, n) => Err([
                ("COMMONS_OIDC_ISSUER", i.is_none()),
                ("COMMONS_OIDC_CLIENT_ID", c.is_none()),
                ("COMMONS_OIDC_CLIENT_SECRET", s.is_none()),
                ("COMMONS_OIDC_NAME", n.is_none()),
            ]
            .into_iter()
            .filter_map(|(k, missing)| missing.then_some(k))
            .collect()),
        }
    }
}

/// Who may create an account through `/v1/auth/register`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    Open,
    Closed,
    /// Only with a valid invite, except the first account on an empty server.
    Invite,
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
            trusted_proxies: vec![],
            log_retention: Duration::from_secs(30 * 24 * 3600),
            idempotency_retention: Duration::from_secs(14 * 24 * 3600),
            job_workers: 4,
            job_lease: Duration::from_secs(90),
            job_poll: Duration::from_millis(500),
            extract_program: None,
            extract_timeout: crate::files::EXTRACT_TIMEOUT,
            files_dir: "./data/files".into(),
            s3_bucket: None,
            max_file_bytes: 2 << 30,
            user_quota_bytes: 20 << 30,
            registration: Registration::Open,
            oidc: None,
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
        c.s3_bucket = var("COMMONS_S3_BUCKET");
        if let Some(v) = var("COMMONS_TRUSTED_PROXIES") {
            c.trusted_proxies = v
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| p.parse())
                .collect::<Result<_, _>>()?;
        }
        if let Some(v) = var("COMMONS_MAX_FILE_BYTES") {
            c.max_file_bytes = v.parse()?;
        }
        if let Some(v) = var("COMMONS_USER_QUOTA_BYTES") {
            c.user_quota_bytes = v.parse()?;
        }
        c.registration = Registration::parse(
            var("COMMONS_REGISTRATION").as_deref(),
            var("COMMONS_REGISTRATION_ALLOWLIST").as_deref(),
        )?;
        c.oidc = match OidcConfig::from_vars(
            var("COMMONS_OIDC_ISSUER"),
            var("COMMONS_OIDC_CLIENT_ID"),
            var("COMMONS_OIDC_CLIENT_SECRET"),
            var("COMMONS_OIDC_NAME"),
        ) {
            Ok(o) => o,
            Err(missing) => {
                tracing::warn!(
                    ?missing,
                    "OIDC sign-in is off: set all four COMMONS_OIDC_* variables"
                );
                None
            }
        };
        Ok(c)
    }
}

impl Registration {
    /// `allowlist` is the setting invites replaced; naming it fails startup with directions.
    pub fn parse(mode: Option<&str>, allowlist: Option<&str>) -> anyhow::Result<Self> {
        let replaced = "the registration allowlist was replaced by invite links: set \
            COMMONS_REGISTRATION=invite, remove COMMONS_REGISTRATION_ALLOWLIST, and invite \
            people from a workspace";
        anyhow::ensure!(allowlist.is_none_or(|a| a.trim().is_empty()), "{replaced}");
        match mode.map(str::trim) {
            None | Some("open") => Ok(Registration::Open),
            Some("closed") => Ok(Registration::Closed),
            Some("invite") => Ok(Registration::Invite),
            Some("allowlist") => anyhow::bail!("{replaced}"),
            Some(other) => {
                anyhow::bail!("COMMONS_REGISTRATION must be open, closed or invite, not {other:?}")
            }
        }
    }
}
