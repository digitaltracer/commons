//! One reader per workspace per node (decision 11). NOTIFY says "workspace W advanced"; the
//! node's feed reads the new rows once and hands the same batch to every local socket, which
//! filters it for its own user. Sockets that fall behind catch up with their own replay.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use proto::events::{WorkspaceEvent, WorkspaceEventBody};
use sqlx::Row;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::AppState;
use crate::sqlx;

const BATCH: i64 = 500;

/// One stored event, serialised once for every socket that receives it.
#[derive(Debug)]
pub struct RawEvent {
    pub seq: i64,
    pub audience: i16,
    pub conversation_id: Option<Uuid>,
    pub subject: Option<Uuid>,
    /// For membership events: (conversation, user, opened, has_access after the change).
    pub membership: Option<(Uuid, Uuid, bool, bool)>,
    /// `workspace_member_left`: the user who left.
    pub left: Option<Uuid>,
    pub joined: Option<Uuid>,
    pub json: String,
}

/// Events in `(from, to]`, read in one snapshot whose head was `head`.
#[derive(Debug)]
pub struct Batch {
    pub workspace_id: Uuid,
    pub from: i64,
    pub to: i64,
    pub head: i64,
    pub events: Vec<RawEvent>,
}

#[derive(Default)]
pub struct Feeds {
    feeds: Mutex<HashMap<Uuid, Arc<Notify>>>,
}

impl Feeds {
    /// Wakes the workspace's feed, starting it on first use.
    pub fn wake(&self, state: &AppState, workspace_id: Uuid) {
        let mut feeds = self.feeds.lock().unwrap_or_else(|p| p.into_inner());
        let notify = feeds.entry(workspace_id).or_insert_with(|| {
            let n = Arc::new(Notify::new());
            tokio::spawn(run(state.clone(), workspace_id, n.clone()));
            n
        });
        notify.notify_one();
    }

    pub fn wake_all(&self) {
        let feeds = self.feeds.lock().unwrap_or_else(|p| p.into_inner());
        for n in feeds.values() {
            n.notify_one();
        }
    }
}

async fn run(state: AppState, workspace_id: Uuid, notify: Arc<Notify>) {
    let mut cursor: Option<i64> = None;
    loop {
        if cursor.is_none() {
            match sqlx::query_scalar::<_, i64>("SELECT seq FROM workspaces WHERE id = $1")
                .bind(workspace_id)
                .fetch_one(&state.pool)
                .await
            {
                // Sockets behind this point catch up through their own replay.
                Ok(head) => cursor = Some(head.saturating_sub(1).max(0)),
                Err(e) => {
                    tracing::warn!(error = ?e, "feed head read failed");
                    notify.notified().await;
                    continue;
                }
            }
        }
        let from = cursor.unwrap_or(0);
        match read(&state, workspace_id, from).await {
            Ok(batch) if batch.to > from => {
                cursor = Some(batch.to);
                let more = batch.to < batch.head;
                state.hub.batch(Arc::new(batch));
                if more {
                    continue;
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = ?e, "feed read failed"),
        }
        notify.notified().await;
    }
}

async fn read(state: &AppState, workspace_id: Uuid, from: i64) -> anyhow::Result<Batch> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let (head, floor): (i64, i64) =
        sqlx::query_as("SELECT seq, log_floor FROM workspaces WHERE id = $1")
            .bind(workspace_id)
            .fetch_one(&mut *tx)
            .await?;
    let from = from.max(floor);
    let to = head.min(from + BATCH);
    let rows = sqlx::query(
        "SELECT seq, audience, conversation_id, subject_user_id, payload FROM events
          WHERE workspace_id = $1 AND seq > $2 AND seq <= $3 ORDER BY seq",
    )
    .bind(workspace_id)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut events = Vec::with_capacity(rows.len());
    for r in rows {
        let seq: i64 = r.get(0);
        let mut body: WorkspaceEventBody = serde_json::from_value(r.get(4))?;
        body.stamp(seq);
        let membership = match &body {
            WorkspaceEventBody::MemberAdded {
                conversation,
                user_id,
                has_access,
                ..
            } => Some((conversation.id, *user_id, true, *has_access)),
            WorkspaceEventBody::MemberRemoved {
                conversation_id,
                user_id,
                has_access,
                ..
            } => Some((*conversation_id, *user_id, false, *has_access)),
            _ => None,
        };
        let left = match &body {
            WorkspaceEventBody::WorkspaceMemberLeft { user_id } => Some(*user_id),
            _ => None,
        };
        let joined = match &body {
            WorkspaceEventBody::WorkspaceMemberJoined { member } => Some(member.user_id),
            _ => None,
        };
        let json = serde_json::to_string(&WorkspaceEvent { seq, body })?;
        events.push(RawEvent {
            seq,
            audience: r.get(1),
            conversation_id: r.get(2),
            subject: r.get(3),
            membership,
            left,
            joined,
            json,
        });
    }
    Ok(Batch {
        workspace_id,
        from,
        to,
        head,
        events,
    })
}

/// What one user can read in one workspace, as of a socket's cursor.
#[derive(Debug, Clone, Default)]
pub struct Access {
    pub open: std::collections::HashSet<Uuid>,
    pub active: bool,
}

impl Access {
    fn step(&mut self, e: &RawEvent, user: Uuid) {
        if let Some((conv, u, _, has_access)) = e.membership
            && u == user
        {
            if has_access {
                self.open.insert(conv);
            } else {
                self.open.remove(&conv);
            }
        }
        if e.left == Some(user) {
            self.active = false;
        }
        if e.joined == Some(user) {
            self.active = true;
        }
    }

    /// Filters a batch exactly as `sync::read_workspace_page` does: a conversation event is
    /// delivered when its seq is inside one of the user's intervals and the user can still
    /// read the conversation at the batch's watermark. Returns the events to send and the
    /// access state after the batch, or `None` when the user is no longer a member.
    pub fn filter<'a>(&self, batch: &'a Batch, user: Uuid) -> (Option<Vec<&'a str>>, Access) {
        let mut end = self.clone();
        for e in &batch.events {
            end.step(e, user);
        }
        if !end.active {
            return (None, end);
        }
        let mut at = self.clone();
        let mut out = vec![];
        for e in &batch.events {
            let deliver = match e.audience {
                0 => true,
                _ if e.subject == Some(user) => true,
                1 => e
                    .conversation_id
                    .is_some_and(|c| at.open.contains(&c) && end.open.contains(&c)),
                _ => false,
            };
            at.step(e, user);
            if deliver {
                out.push(e.json.as_str());
            }
        }
        (Some(out), end)
    }
}

/// Loads a user's open intervals in a workspace, inside the caller's snapshot.
pub async fn load_access(
    conn: &mut sqlx::PgConnection,
    workspace_id: Uuid,
    user_id: Uuid,
) -> sqlx::Result<Access> {
    let open: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT ai.conversation_id FROM access_intervals ai
           JOIN conversations c ON c.id = ai.conversation_id
          WHERE c.workspace_id = $1 AND ai.user_id = $2 AND ai.left_seq IS NULL
            AND ai.joined_seq IS NOT NULL",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workspace_members WHERE workspace_id = $1
            AND user_id = $2 AND left_seq IS NULL)",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Access {
        open: open.into_iter().collect(),
        active,
    })
}
