//! In-process real-time fan-out (decision 11).
//!
//! The hub carries wakeups, not content: Postgres NOTIFY says "stream X advanced", and each
//! socket task reads and filters the rows itself. A bounded queue per connection; a full
//! queue disconnects the client, which resumes from its seqs.
use crate::sqlx;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sqlx::postgres::PgListener;
use proto::events::ServerFrame;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::AppState;

pub const CHANNEL: &str = "commons";

#[derive(Debug, Clone)]
pub enum Wakeup {
    Workspace(Uuid),
    User,
    Ephemeral(Ephemeral),
    /// Re-read every stream; sent after the NOTIFY listener reconnects.
    Repair,
}

/// Presence and typing: never logged, delivered only to sockets connected now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ephemeral {
    pub workspace_id: Uuid,
    pub frame: ServerFrame,
}

struct Conn {
    user_id: Uuid,
    tx: mpsc::Sender<Wakeup>,
    cancel: CancellationToken,
    workspaces: HashSet<Uuid>,
}

#[derive(Default)]
struct HubInner {
    conns: HashMap<u64, Conn>,
    by_user: HashMap<Uuid, HashSet<u64>>,
    by_workspace: HashMap<Uuid, HashSet<u64>>,
}

pub struct Hub {
    inner: Mutex<HubInner>,
    queue: usize,
    next_id: AtomicU64,
    /// Sockets disconnected because they could not keep up.
    pub dropped_slow: AtomicU64,
}

pub struct Registration {
    pub id: u64,
    pub rx: mpsc::Receiver<Wakeup>,
    pub cancel: CancellationToken,
}

impl Hub {
    pub fn new(queue: usize) -> Self {
        Hub {
            inner: Mutex::new(HubInner::default()),
            queue: queue.max(1),
            next_id: AtomicU64::new(1),
            dropped_slow: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn register(&self, user_id: Uuid) -> Registration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(self.queue);
        let cancel = CancellationToken::new();
        let mut inner = self.lock();
        inner.conns.insert(
            id,
            Conn {
                user_id,
                tx,
                cancel: cancel.clone(),
                workspaces: HashSet::new(),
            },
        );
        inner.by_user.entry(user_id).or_default().insert(id);
        Registration { id, rx, cancel }
    }

    pub fn set_workspaces(&self, id: u64, workspaces: impl IntoIterator<Item = Uuid>) {
        let mut inner = self.lock();
        let new: HashSet<Uuid> = workspaces.into_iter().collect();
        let old = match inner.conns.get_mut(&id) {
            Some(c) => std::mem::replace(&mut c.workspaces, new.clone()),
            None => return,
        };
        for w in old.difference(&new) {
            if let Some(s) = inner.by_workspace.get_mut(w) {
                s.remove(&id);
            }
        }
        for w in new {
            inner.by_workspace.entry(w).or_default().insert(id);
        }
    }

    /// Removes a connection. Returns true when it was the user's last one.
    pub fn unregister(&self, id: u64) -> bool {
        let mut inner = self.lock();
        let Some(conn) = inner.conns.remove(&id) else {
            return false;
        };
        for w in &conn.workspaces {
            if let Some(s) = inner.by_workspace.get_mut(w) {
                s.remove(&id);
            }
        }
        let last = match inner.by_user.get_mut(&conn.user_id) {
            Some(s) => {
                s.remove(&id);
                s.is_empty()
            }
            None => true,
        };
        if last {
            inner.by_user.remove(&conn.user_id);
        }
        last
    }

    pub fn connections(&self, user_id: Uuid) -> usize {
        self.lock().by_user.get(&user_id).map_or(0, |s| s.len())
    }

    pub fn online_in(&self, workspace_id: Uuid) -> Vec<Uuid> {
        let inner = self.lock();
        let mut users: Vec<Uuid> = inner
            .by_workspace
            .get(&workspace_id)
            .into_iter()
            .flatten()
            .filter_map(|id| inner.conns.get(id).map(|c| c.user_id))
            .collect();
        users.sort();
        users.dedup();
        users
    }

    fn deliver(&self, inner: &HubInner, ids: impl IntoIterator<Item = u64>, w: &Wakeup) {
        for id in ids {
            let Some(conn) = inner.conns.get(&id) else {
                continue;
            };
            if let Err(mpsc::error::TrySendError::Full(_)) = conn.tx.try_send(w.clone()) {
                tracing::info!(conn = id, user = %conn.user_id, "slow consumer disconnected");
                self.dropped_slow.fetch_add(1, Ordering::Relaxed);
                conn.cancel.cancel();
            }
        }
    }

    pub fn wake_workspace(&self, workspace_id: Uuid) {
        let inner = self.lock();
        let ids: Vec<u64> = inner
            .by_workspace
            .get(&workspace_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        self.deliver(&inner, ids, &Wakeup::Workspace(workspace_id));
    }

    pub fn wake_user(&self, user_id: Uuid) {
        let inner = self.lock();
        let ids: Vec<u64> = inner
            .by_user
            .get(&user_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        self.deliver(&inner, ids, &Wakeup::User);
    }

    pub fn ephemeral(&self, e: Ephemeral) {
        let inner = self.lock();
        let ids: Vec<u64> = inner
            .by_workspace
            .get(&e.workspace_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        self.deliver(&inner, ids, &Wakeup::Ephemeral(e));
    }

    pub fn wake_all(&self) {
        let inner = self.lock();
        let ids: Vec<u64> = inner.conns.keys().copied().collect();
        self.deliver(&inner, ids, &Wakeup::Repair);
    }

    pub fn dispatch(&self, payload: &str) {
        let Some((kind, rest)) = payload.split_once(':') else {
            return;
        };
        match kind {
            "w" => {
                if let Ok(id) = rest.parse() {
                    self.wake_workspace(id)
                }
            }
            "u" => {
                if let Ok(id) = rest.parse() {
                    self.wake_user(id)
                }
            }
            "e" => {
                if let Ok(e) = serde_json::from_str(rest) {
                    self.ephemeral(e)
                }
            }
            _ => {}
        }
    }
}

/// Publishes presence or typing to every node through NOTIFY. Not transactional, not logged.
pub async fn publish_ephemeral(pool: &sqlx::PgPool, e: &Ephemeral) {
    let payload = match serde_json::to_string(e) {
        Ok(p) => format!("e:{p}"),
        Err(_) => return,
    };
    if let Err(err) = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload)
        .execute(pool)
        .await
    {
        tracing::warn!(error = ?err, "ephemeral publish failed");
    }
}

/// Feeds NOTIFY wakeups into the hub. After a reconnect every socket re-reads its streams,
/// so a lost notification costs latency, never data.
pub async fn listen(state: AppState, cancel: CancellationToken) {
    loop {
        let mut listener = match PgListener::connect_with(&state.pool).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = ?e, "notify listener connect failed");
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => continue,
                }
            }
        };
        if let Err(e) = listener.listen(CHANNEL).await {
            tracing::warn!(error = ?e, "LISTEN failed");
            continue;
        }
        state.hub.wake_all();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                n = listener.try_recv() => match n {
                    Ok(Some(n)) => state.hub.dispatch(n.payload()),
                    Ok(None) => {
                        tracing::warn!("notify listener reconnecting");
                        state.hub.wake_all();
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "notify listener error");
                        break;
                    }
                }
            }
        }
    }
}
