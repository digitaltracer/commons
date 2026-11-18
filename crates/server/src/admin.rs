//! `commonsd admin …`: account recovery for the server operator, run on the host with the
//! server's own environment. Workspace admins can't do these, because accounts span every
//! workspace on the server.
use crate::sqlx;
use crate::sqlx::{PgConnection, PgPool};
use proto::model::{Role, Workspace};
use uuid::Uuid;

use crate::Config;
use crate::auth::{hash_token, new_token, revoke_user_sessions};

pub const USAGE: &str = "usage: commonsd admin <command>

  list-users                       every account, with its state
  reset-password <email>           print a one-time reset link (24 hours) and sign the account out
  revoke-sessions <email>          sign the account out everywhere
  deactivate <email>               block sign-in and sign the account out
  reactivate <email>               allow sign-in again
  make-owner <email> <workspace>   make the account an owner (workspace id or exact name)";

const RESET_HOURS: i32 = 24;

/// Runs one command and returns what to print. Each command also logs what it did.
pub async fn run(pool: &PgPool, config: &Config, args: &[String]) -> anyhow::Result<String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["list-users"] => list_users(pool).await,
        ["reset-password", email] => reset_password(pool, config, email).await,
        ["revoke-sessions", email] => revoke_sessions(pool, email).await,
        ["deactivate", email] => set_active(pool, email, false).await,
        ["reactivate", email] => set_active(pool, email, true).await,
        ["make-owner", email, workspace] => make_owner(pool, email, workspace).await,
        _ => anyhow::bail!("{USAGE}"),
    }
}

async fn find_user(conn: &mut PgConnection, email: &str) -> anyhow::Result<(Uuid, String)> {
    let email = email.trim().to_lowercase();
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&mut *conn)
        .await?;
    let id = id.ok_or_else(|| anyhow::anyhow!("no account has the email {email}"))?;
    Ok((id, email))
}

/// Email, name, has a password, identities, deactivated, workspaces, live sessions, created.
type UserRow = (
    String,
    String,
    bool,
    i64,
    bool,
    i64,
    i64,
    time::OffsetDateTime,
);

async fn list_users(pool: &PgPool) -> anyhow::Result<String> {
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT u.email, u.display_name, u.password_hash IS NOT NULL,
                    (SELECT count(*) FROM oidc_identities i WHERE i.user_id = u.id),
                    u.deactivated_at IS NOT NULL,
                    (SELECT count(*) FROM workspace_members m
                      WHERE m.user_id = u.id AND m.left_seq IS NULL),
                    (SELECT count(*) FROM sessions s
                      WHERE s.user_id = u.id AND s.revoked_at IS NULL),
                    u.created_at
               FROM users u ORDER BY u.created_at",
    )
    .fetch_all(pool)
    .await?;
    let mut out = format!(
        "{:<36} {:<24} {:<10} {:<16} {:>10} {:>8}  {}\n",
        "EMAIL", "NAME", "STATE", "SIGN-IN", "WORKSPACES", "SESSIONS", "CREATED"
    );
    for (email, name, password, oidc, deactivated, workspaces, sessions, created) in &rows {
        let sign_in = match (*password, *oidc) {
            (true, 0) => "password".to_string(),
            (false, n) => format!("oidc×{n}"),
            (true, n) => format!("password+oidc×{n}"),
        };
        let created = created
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        out += &format!(
            "{email:<36} {name:<24} {:<10} {sign_in:<16} {workspaces:>10} {sessions:>8}  {created}\n",
            if *deactivated {
                "deactivated"
            } else {
                "active"
            },
        );
    }
    out += &format!("{} accounts", rows.len());
    tracing::info!(target: "commons::admin", accounts = rows.len(), "list-users");
    Ok(out)
}

/// Issues a single-use reset link, as an emailed reset does, and revokes every session.
async fn reset_password(pool: &PgPool, config: &Config, email: &str) -> anyhow::Result<String> {
    let mut tx = pool.begin().await?;
    let (user_id, email) = find_user(&mut tx, email).await?;
    let token = new_token();
    sqlx::query(
        "INSERT INTO password_resets (token_hash, user_id, expires_at)
         VALUES ($1, $2, now() + make_interval(hours => $3))",
    )
    .bind(&hash_token(&token)[..])
    .bind(user_id)
    .bind(RESET_HOURS)
    .execute(&mut *tx)
    .await?;
    let revoked = revoke_user_sessions(&mut tx, user_id).await?;
    tx.commit().await?;
    tracing::info!(target: "commons::admin", %email, %user_id, revoked,
        "reset-password: issued a reset link and revoked sessions");
    let link = format!("{}/reset#{token}", config.public_url.trim_end_matches('/'));
    Ok(format!(
        "Reset link for {email}. It works once, for {RESET_HOURS} hours. Send it privately:\n\
         {link}\nRevoked {revoked} session(s)."
    ))
}

async fn revoke_sessions(pool: &PgPool, email: &str) -> anyhow::Result<String> {
    let mut tx = pool.begin().await?;
    let (user_id, email) = find_user(&mut tx, email).await?;
    let revoked = revoke_user_sessions(&mut tx, user_id).await?;
    tx.commit().await?;
    tracing::info!(target: "commons::admin", %email, %user_id, revoked, "revoke-sessions");
    Ok(format!("Revoked {revoked} session(s) of {email}."))
}

/// Deactivating revokes the sessions too, which closes the account's sockets on every node.
async fn set_active(pool: &PgPool, email: &str, active: bool) -> anyhow::Result<String> {
    let mut tx = pool.begin().await?;
    let (user_id, email) = find_user(&mut tx, email).await?;
    let changed = sqlx::query(if active {
        "UPDATE users SET deactivated_at = NULL WHERE id = $1 AND deactivated_at IS NOT NULL"
    } else {
        "UPDATE users SET deactivated_at = now() WHERE id = $1 AND deactivated_at IS NULL"
    })
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    let revoked = if active {
        0
    } else {
        revoke_user_sessions(&mut tx, user_id).await?
    };
    tx.commit().await?;
    let command = if active { "reactivate" } else { "deactivate" };
    tracing::info!(target: "commons::admin", %email, %user_id, changed, revoked, command);
    Ok(match (active, changed) {
        (true, true) => format!("Reactivated {email}. They can sign in again."),
        (true, false) => format!("{email} was already active."),
        (false, true) => format!("Deactivated {email} and revoked {revoked} session(s)."),
        (false, false) => {
            format!("{email} was already deactivated; revoked {revoked} remaining session(s).")
        }
    })
}

async fn find_workspace(conn: &mut PgConnection, key: &str) -> anyhow::Result<Workspace> {
    let rows: Vec<(Uuid, String)> = match Uuid::parse_str(key.trim()) {
        Ok(id) => {
            sqlx::query_as("SELECT id, name FROM workspaces WHERE id = $1")
                .bind(id)
                .fetch_all(&mut *conn)
                .await?
        }
        Err(_) => {
            sqlx::query_as("SELECT id, name FROM workspaces WHERE name = $1")
                .bind(key.trim())
                .fetch_all(&mut *conn)
                .await?
        }
    };
    match rows.as_slice() {
        [(id, name)] => Ok(Workspace {
            id: *id,
            name: name.clone(),
        }),
        [] => anyhow::bail!("no workspace is called or has the id {key:?}"),
        many => anyhow::bail!(
            "several workspaces are called {key:?}; use one of their ids: {}",
            many.iter()
                .map(|(id, _)| id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Makes the account an owner, joining it to the workspace first if it isn't a member.
async fn make_owner(pool: &PgPool, email: &str, workspace: &str) -> anyhow::Result<String> {
    let mut tx = pool.begin().await?;
    let (user_id, email) = find_user(&mut tx, email).await?;
    let ws = find_workspace(&mut tx, workspace).await?;
    crate::sync::lock_workspace(&mut tx, ws.id).await?;
    let old = crate::sync::active_role(&mut tx, ws.id, user_id).await?;
    match old {
        Some(old) => crate::directory::change_role(&mut tx, ws.id, user_id, old, Role::Owner)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        None => {
            let events = crate::directory::join(&mut tx, ws.id, user_id, Role::Owner)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            crate::sync::append_join(&mut tx, &ws, user_id, events)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    tx.commit().await?;
    let was = old.map_or("not a member", |r| r.as_str());
    tracing::info!(target: "commons::admin", %email, %user_id, workspace = %ws.id, was,
        "make-owner");
    Ok(match old {
        Some(Role::Owner) => format!("{email} is already an owner of {} ({}).", ws.name, ws.id),
        _ => format!(
            "{email} is now an owner of {} ({}); was {was}.",
            ws.name, ws.id
        ),
    })
}
