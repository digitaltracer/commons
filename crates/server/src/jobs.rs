//! Background work through the `jobs` table (decision 7).
//!
//! A worker claims a job with a lease. If the worker dies, the lease expires and another
//! worker reclaims the job, so a killed job is retried rather than lost.
use crate::sqlx;
use std::sync::Arc;
use std::time::Duration;

use crate::sqlx::Row;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::AppState;

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub attempts: i32,
    pub max_attempts: i32,
}

pub async fn enqueue(
    conn: &mut sqlx::PgConnection,
    kind: &str,
    payload: serde_json::Value,
) -> sqlx::Result<i64> {
    sqlx::query_scalar("INSERT INTO jobs (kind, payload) VALUES ($1, $2) RETURNING id")
        .bind(kind)
        .bind(payload)
        .fetch_one(&mut *conn)
        .await
}

/// Claims the next runnable job, taking a lease of `lease`.
pub async fn claim(pool: &sqlx::PgPool, lease: Duration) -> sqlx::Result<Option<Job>> {
    let row = sqlx::query(
        "UPDATE jobs SET locked_until = now() + make_interval(secs => $1), attempts = attempts + 1
          WHERE id = (
              SELECT id FROM jobs
               WHERE done_at IS NULL AND failed_at IS NULL AND run_at <= now()
                 AND (locked_until IS NULL OR locked_until < now())
               ORDER BY run_at, id
               FOR UPDATE SKIP LOCKED LIMIT 1)
          RETURNING id, kind, payload, attempts, max_attempts",
    )
    .bind(lease.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Job {
        id: r.get("id"),
        kind: r.get("kind"),
        payload: r.get("payload"),
        attempts: r.get("attempts"),
        max_attempts: r.get("max_attempts"),
    }))
}

pub async fn complete(pool: &sqlx::PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET done_at = now(), locked_until = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records a failure. The job is retried with backoff until `max_attempts`, then marked failed.
pub async fn fail(pool: &sqlx::PgPool, job: &Job, error: &str) -> sqlx::Result<()> {
    let backoff = 2f64.powi(job.attempts.min(10)).min(600.0);
    sqlx::query(
        "UPDATE jobs SET locked_until = NULL, last_error = $2,
                run_at = now() + make_interval(secs => $3),
                failed_at = CASE WHEN attempts >= max_attempts THEN now() END
          WHERE id = $1",
    )
    .bind(job.id)
    .bind(error)
    .bind(backoff)
    .execute(pool)
    .await?;
    Ok(())
}

/// Runs one job to completion or failure.
pub async fn run_one(state: &AppState, job: Job) {
    let result = dispatch(state, &job).await;
    let outcome = match result {
        Ok(()) => complete(&state.pool, job.id).await,
        Err(e) => {
            tracing::warn!(job = job.id, kind = %job.kind, error = ?e, "job failed");
            fail(&state.pool, &job, &format!("{e:#}")).await
        }
    };
    if let Err(e) = outcome {
        tracing::warn!(job = job.id, error = ?e, "job bookkeeping failed");
    }
}

async fn dispatch(state: &AppState, job: &Job) -> anyhow::Result<()> {
    crate::jobs::handle(state, job).await
}

/// Job handlers by kind. Later steps add notify, extraction and thumbnails.
pub async fn handle(_state: &AppState, job: &Job) -> anyhow::Result<()> {
    anyhow::bail!("unknown job kind {}", job.kind)
}

/// Polls for jobs with at most `job_workers` running at once, plus hourly maintenance.
pub async fn run_workers(state: AppState, cancel: CancellationToken) {
    let permits = Arc::new(Semaphore::new(state.config.job_workers.max(1)));
    let mut maintenance = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = maintenance.tick() => {
                if let Err(e) = maintain(&state).await {
                    tracing::warn!(error = ?e, "maintenance failed");
                }
            }
            permit = permits.clone().acquire_owned() => {
                let Ok(permit) = permit else { return };
                match claim(&state.pool, state.config.job_lease).await {
                    Ok(Some(job)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            run_one(&state, job).await;
                            drop(permit);
                        });
                    }
                    Ok(None) => {
                        drop(permit);
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(state.config.job_poll) => {}
                        }
                    }
                    Err(e) => {
                        drop(permit);
                        tracing::warn!(error = ?e, "job claim failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
    }
}

/// Prunes expired idempotency keys and old finished jobs.
pub async fn maintain(state: &AppState) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM idempotency_keys WHERE created_at < now() - make_interval(secs => $1)",
    )
    .bind(state.config.idempotency_retention.as_secs_f64())
    .execute(&state.pool)
    .await?;
    sqlx::query("DELETE FROM jobs WHERE done_at < now() - interval '7 days'")
        .execute(&state.pool)
        .await?;
    crate::sync::prune(state).await?;
    Ok(())
}
