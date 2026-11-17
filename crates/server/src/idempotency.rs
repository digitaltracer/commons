//! Client-generated keys that make retried commands no-ops (decision 5).
use crate::sqlx;
use crate::sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;

pub enum Claim {
    /// First time this key is seen; the caller performs the command.
    Fresh,
    /// Seen before; the stored response is returned instead.
    Replay(serde_json::Value),
}

/// Claims `key` inside the caller's transaction. A concurrent duplicate blocks on the
/// uncommitted row and then sees the first request's stored response.
pub async fn claim(conn: &mut PgConnection, user_id: Uuid, key: Uuid) -> ApiResult<Claim> {
    let inserted = sqlx::query(
        "INSERT INTO idempotency_keys (user_id, key, response) VALUES ($1, $2, 'null')
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(key)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(Claim::Fresh);
    }
    let response: serde_json::Value =
        sqlx::query_scalar("SELECT response FROM idempotency_keys WHERE user_id = $1 AND key = $2")
            .bind(user_id)
            .bind(key)
            .fetch_one(&mut *conn)
            .await?;
    Ok(Claim::Replay(response))
}

pub async fn store(
    conn: &mut PgConnection,
    user_id: Uuid,
    key: Uuid,
    response: &serde_json::Value,
) -> ApiResult<()> {
    sqlx::query("UPDATE idempotency_keys SET response = $3 WHERE user_id = $1 AND key = $2")
        .bind(user_id)
        .bind(key)
        .bind(response)
        .execute(&mut *conn)
        .await?;
    Ok(())
}
