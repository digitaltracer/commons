use crate::sqlx::PgPool;
use crate::sqlx::postgres::PgPoolOptions;

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(32)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await?)
}

/// Migrations, embedded so `commonsd` is a single self-contained binary.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "init", include_str!("../migrations/0001_init.sql")),
    (
        2,
        "user stream",
        include_str!("../migrations/0002_user_stream.sql"),
    ),
    (
        3,
        "messaging",
        include_str!("../migrations/0003_messaging.sql"),
    ),
];

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    use std::borrow::Cow;

    use crate::sqlx::migrate::{Migration, MigrationType, Migrator};
    let migrator = Migrator {
        migrations: Cow::Owned(
            MIGRATIONS
                .iter()
                .map(|(v, d, sql)| {
                    Migration::new(
                        *v,
                        Cow::Borrowed(d),
                        MigrationType::Simple,
                        Cow::Borrowed(sql),
                        false,
                    )
                })
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    migrator.run(pool).await?;
    Ok(())
}

pub fn ms(t: time::OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}
