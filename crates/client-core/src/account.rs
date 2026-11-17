//! One signed-in server account: its cache, sync engine, outbox and windows.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use proto::api::{MemberList, MessagePage};
use proto::events::{
    ClientFrame, ServerFrame, StreamCursor, UserEvent, UserEventBody, UserReplay, UserSnapshot,
    WorkspaceEvent, WorkspaceReplay,
};
use proto::model::{Message, Notification, User};
use proto::view::{AccountView, ConnectionState, DraftReason, DraftView, MessageView};
use proto::window::WindowSpec;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::apply;
use crate::platform::Notifier;
use crate::store::{Backfill, Changes, Op, OutboxItem, OutboxKind, OutboxState, Storage};
use crate::transport::{Method, Transport, TransportError, call};
use crate::views::{self, Names};
use crate::window::{RowSource, Sink, WindowServer};

/// Sends that cannot be delivered for this long give up and surface as failed.
pub const OUTBOX_GIVE_UP: Duration = Duration::from_secs(7 * 24 * 3600);
const BACKFILL_PAGE: i64 = 100;

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub id: Uuid,
    pub server_url: String,
    pub user: User,
    pub data_dir: PathBuf,
}

/// Low-volume notifications from an account to whoever hosts it.
#[derive(Debug, Clone)]
pub enum AccountEvent {
    Sidebar,
    Drafts,
    Typing {
        conversation_id: Uuid,
        names: Vec<String>,
    },
    Presence {
        online: Vec<Uuid>,
    },
    Notification(Notification),
}

pub type EventSink = Arc<dyn Fn(Uuid, AccountEvent) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("signed out")]
    SignedOut,
    #[error("this server needs a newer client")]
    UpgradeRequired,
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SyncError {
    fn from_transport(e: TransportError) -> SyncError {
        match e.status() {
            Some(401) => SyncError::SignedOut,
            Some(426) => SyncError::UpgradeRequired,
            _ => SyncError::Transport(e),
        }
    }
}

/// Test hooks that simulate a crash at named points.
#[cfg(feature = "testing")]
#[derive(Default)]
pub struct Faults {
    /// Fail before committing a page that grants access (before the backfill marker exists).
    pub crash_before_grant_commit: std::sync::atomic::AtomicBool,
    /// Fail after this many backfill pages have been committed.
    pub crash_after_backfill_pages: std::sync::atomic::AtomicI64,
}

pub struct Inner {
    pub id: Uuid,
    pub server_url: String,
    pub user: User,
    transport: Arc<dyn Transport>,
    token: RwLock<Option<String>>,
    pub store: Arc<dyn Storage>,
    pub windows: WindowServer,
    notifier: Arc<dyn Notifier>,
    events: EventSink,
    connection: Mutex<ConnectionState>,
    focus: Mutex<Option<Uuid>>,
    wake_backfill: Notify,
    wake_outbox: Notify,
    wake_flush: Notify,
    cancel: CancellationToken,
    socket_tx: Mutex<Option<mpsc::Sender<String>>>,
    typing: Mutex<HashMap<Uuid, HashMap<Uuid, Instant>>>,
    online: Mutex<HashSet<Uuid>>,
    /// Serialises stream application so a cursor never moves backwards.
    apply_lock: Mutex<()>,
    #[cfg(feature = "testing")]
    pub faults: Faults,
    /// Every workspace event this account received, for permission tests.
    #[cfg(feature = "testing")]
    pub received: Mutex<Vec<(Uuid, WorkspaceEvent)>>,
}

#[derive(Clone)]
pub struct Account(pub Arc<Inner>);

impl std::ops::Deref for Account {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn version_tuple(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

impl Account {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AccountConfig,
        transport: Arc<dyn Transport>,
        store: Arc<dyn Storage>,
        token: Option<String>,
        notifier: Arc<dyn Notifier>,
        events: EventSink,
    ) -> Account {
        Account(Arc::new(Inner {
            id: config.id,
            server_url: config.server_url,
            user: config.user,
            transport,
            token: RwLock::new(token),
            store,
            windows: WindowServer::default(),
            notifier,
            events,
            connection: Mutex::new(ConnectionState::Offline),
            focus: Mutex::new(None),
            wake_backfill: Notify::new(),
            wake_outbox: Notify::new(),
            wake_flush: Notify::new(),
            cancel: CancellationToken::new(),
            socket_tx: Mutex::new(None),
            typing: Mutex::new(HashMap::new()),
            online: Mutex::new(HashSet::new()),
            apply_lock: Mutex::new(()),
            #[cfg(feature = "testing")]
            faults: Faults::default(),
            #[cfg(feature = "testing")]
            received: Mutex::new(vec![]),
        }))
    }

    fn token(&self) -> Result<String, SyncError> {
        self.token
            .read()
            .ok()
            .and_then(|t| t.clone())
            .ok_or(SyncError::SignedOut)
    }

    pub fn connection(&self) -> ConnectionState {
        self.connection
            .lock()
            .map(|c| c.clone())
            .unwrap_or(ConnectionState::Offline)
    }

    fn set_connection(&self, c: ConnectionState) {
        let changed = match self.connection.lock() {
            Ok(mut cur) if *cur != c => {
                *cur = c;
                true
            }
            _ => false,
        };
        if changed {
            (self.events)(self.id, AccountEvent::Sidebar);
        }
    }

    pub async fn api<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&impl serde::Serialize>,
        key: Option<Uuid>,
    ) -> Result<T, SyncError> {
        let token = self.token()?;
        call(
            &*self.transport,
            method,
            path,
            query,
            body,
            Some(&token),
            key,
        )
        .await
        .map_err(SyncError::from_transport)
    }

    pub fn view(&self) -> anyhow::Result<AccountView> {
        Ok(AccountView {
            id: self.id,
            server_url: self.server_url.clone(),
            email: self.user.email.clone(),
            display_name: self.user.display_name.clone(),
            user_id: self.user.id,
            connection: self.connection(),
            workspaces: views::workspace_views(&*self.store, self.user.id)?,
        })
    }

    /// Commits a batch and tells windows and the host what changed.
    pub fn commit(&self, ops: Vec<Op>) -> anyhow::Result<Changes> {
        let changes = self.store.commit(ops)?;
        self.changed(&changes);
        Ok(changes)
    }

    fn changed(&self, changes: &Changes) {
        if !changes.conversations.is_empty() {
            self.windows.invalidate(&changes.conversations);
            self.wake_flush.notify_one();
        }
        if changes.sidebar {
            (self.events)(self.id, AccountEvent::Sidebar);
        }
        if changes.drafts {
            (self.events)(self.id, AccountEvent::Drafts);
        }
    }

    // ---- sync ----

    async fn check_version(&self) -> Result<(), SyncError> {
        let meta: proto::api::Meta = call(
            &*self.transport,
            Method::Get,
            "/meta",
            &[],
            None::<&()>,
            None,
            None,
        )
        .await
        .map_err(SyncError::from_transport)?;
        if version_tuple(&meta.min_client_version) > version_tuple(proto::CLIENT_VERSION) {
            return Err(SyncError::UpgradeRequired);
        }
        Ok(())
    }

    /// Replays the user's own stream: workspace joins and removals, read cursors,
    /// preferences and notifications.
    pub async fn catch_up_user(&self) -> Result<(), SyncError> {
        loop {
            let since = self.store.user_since()?;
            let page: UserReplay = self
                .api(
                    Method::Get,
                    "/sync/user",
                    &[("since", since.to_string())],
                    None::<&()>,
                    None,
                )
                .await?;
            let added = match &page.snapshot {
                Some(snapshot) => self.apply_user_snapshot(snapshot)?,
                None => self.apply_user_page(&page.events, page.next_since)?,
            };
            for w in added {
                self.fetch_members(w).await?;
            }
            if page.snapshot.is_none() && page.next_since >= page.head {
                return Ok(());
            }
        }
    }

    /// Applies user events and advances the user cursor in one commit. Returns the
    /// workspaces that were added, whose member lists still need fetching.
    fn apply_user_page(
        &self,
        events: &[UserEvent],
        next_since: i64,
    ) -> Result<Vec<Uuid>, SyncError> {
        let _guard = self.apply_lock.lock().unwrap_or_else(|p| p.into_inner());
        let since = self.store.user_since()?;
        // Membership as of each event in the page: a removal and a re-join can share a page.
        let mut present: HashSet<Uuid> =
            self.store.workspaces()?.iter().map(|(w, _)| w.id).collect();
        let mut ops = vec![];
        let mut added = vec![];
        let mut notifications = vec![];
        for ev in events.iter().filter(|e| e.seq > since) {
            match &ev.body {
                UserEventBody::WorkspaceJoined {
                    workspace,
                    start_seq,
                } => {
                    if present.insert(workspace.id) {
                        added.push(workspace.id);
                        ops.push(Op::AddWorkspace {
                            workspace: workspace.clone(),
                            since: (start_seq - 1).max(0),
                        });
                    }
                }
                UserEventBody::WorkspaceRemoved { workspace_id } => {
                    present.remove(workspace_id);
                    added.retain(|w| w != workspace_id);
                    ops.push(Op::RemoveWorkspace(*workspace_id));
                }
                UserEventBody::ReadCursor {
                    conversation_id,
                    seq,
                    ..
                } => ops.push(Op::SetReadCursor {
                    conversation_id: *conversation_id,
                    seq: *seq,
                }),
                UserEventBody::PreferencesChanged { preferences } => ops.push(Op::SetMeta(
                    "preferences".into(),
                    serde_json::to_string(preferences).map_err(anyhow::Error::from)?,
                )),
                UserEventBody::Notify { notification } => notifications.push(notification.clone()),
            }
        }
        ops.push(Op::SetUserSince(next_since.max(since)));
        self.commit(ops)?;
        for n in notifications {
            present_notification(self, &n);
        }
        Ok(added)
    }

    fn apply_user_snapshot(&self, snapshot: &UserSnapshot) -> Result<Vec<Uuid>, SyncError> {
        let _guard = self.apply_lock.lock().unwrap_or_else(|p| p.into_inner());
        let known: HashSet<Uuid> = self.store.workspaces()?.iter().map(|(w, _)| w.id).collect();
        let current: HashSet<Uuid> = snapshot.workspaces.iter().map(|(w, _)| w.id).collect();
        let mut ops: Vec<Op> = known
            .difference(&current)
            .map(|w| Op::RemoveWorkspace(*w))
            .collect();
        let mut added = vec![];
        for (w, resume) in &snapshot.workspaces {
            if !known.contains(&w.id) {
                added.push(w.id);
                ops.push(Op::AddWorkspace {
                    workspace: w.clone(),
                    since: *resume,
                });
            }
        }
        for c in &snapshot.read_cursors {
            ops.push(Op::SetReadCursor {
                conversation_id: c.conversation_id,
                seq: c.seq,
            });
        }
        ops.push(Op::SetMeta(
            "preferences".into(),
            serde_json::to_string(&snapshot.preferences).map_err(anyhow::Error::from)?,
        ));
        ops.push(Op::SetUserSince(snapshot.seq));
        self.commit(ops)?;
        Ok(added)
    }

    async fn fetch_members(&self, workspace_id: Uuid) -> Result<(), SyncError> {
        let members: MemberList = match self
            .api(
                Method::Get,
                &format!("/workspaces/{workspace_id}/members"),
                &[],
                None::<&()>,
                None,
            )
            .await
        {
            Ok(m) => m,
            Err(SyncError::Transport(e)) if e.status() == Some(403) => return Ok(()),
            Err(e) => return Err(e),
        };
        self.commit(apply::members(members.members))?;
        Ok(())
    }

    /// Applies one replay page (or socket frame). Each event commits together with the cursor
    /// advance past it, so each decision sees the state the previous event left, and a pending
    /// backfill is never written without its cursor (or the cursor without the backfill).
    pub fn apply_workspace_page(
        &self,
        workspace_id: Uuid,
        events: &[WorkspaceEvent],
        next_since: i64,
    ) -> Result<(), SyncError> {
        let _guard = self.apply_lock.lock().unwrap_or_else(|p| p.into_inner());
        let since = self
            .store
            .workspaces()?
            .into_iter()
            .find(|(w, _)| w.id == workspace_id)
            .map(|(_, s)| s);
        let Some(mut since) = since else {
            return Ok(());
        };
        let mut grants = false;
        for ev in events {
            if ev.seq <= since {
                continue;
            }
            #[cfg(feature = "testing")]
            if let Ok(mut r) = self.received.lock() {
                r.push((workspace_id, ev.clone()));
            }
            let mut ops = apply::workspace_event(&*self.store, self.user.id, workspace_id, ev)?;
            let grant = ops.iter().any(|o| matches!(o, Op::AddBackfill(_)));
            #[cfg(feature = "testing")]
            if grant
                && self
                    .faults
                    .crash_before_grant_commit
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(SyncError::Other(anyhow::anyhow!(
                    "injected crash before grant commit"
                )));
            }
            ops.push(Op::SetWorkspaceSince(workspace_id, ev.seq));
            self.commit(ops)?;
            since = ev.seq;
            grants |= grant;
        }
        if next_since > since {
            self.commit(vec![Op::SetWorkspaceSince(workspace_id, next_since)])?;
        }
        if grants {
            self.wake_backfill.notify_one();
        }
        Ok(())
    }

    /// Replays a workspace stream over HTTPS until caught up, taking a snapshot if needed.
    pub async fn catch_up_workspace(&self, workspace_id: Uuid) -> Result<(), SyncError> {
        loop {
            let since = self
                .store
                .workspaces()?
                .into_iter()
                .find(|(w, _)| w.id == workspace_id)
                .map(|(_, s)| s);
            let Some(since) = since else { return Ok(()) };
            let page: WorkspaceReplay = match self
                .api(
                    Method::Get,
                    &format!("/sync/workspaces/{workspace_id}"),
                    &[("since", since.to_string())],
                    None::<&()>,
                    None,
                )
                .await
            {
                Ok(p) => p,
                Err(SyncError::Transport(e)) if e.status() == Some(403) => {
                    // No longer a member; the user stream's workspace_removed drives the purge.
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if let Some(snapshot) = &page.snapshot {
                let _guard = self.apply_lock.lock().unwrap_or_else(|p| p.into_inner());
                self.commit(apply::workspace_snapshot(snapshot))?;
                self.wake_backfill.notify_one();
                continue;
            }
            self.apply_workspace_page(workspace_id, &page.events, page.next_since)?;
            if page.next_since >= page.head {
                return Ok(());
            }
        }
    }

    /// One full HTTPS sync: workspaces, every stream, then pending backfills.
    pub async fn sync_once(&self) -> Result<(), SyncError> {
        self.catch_up_user().await?;
        for (w, _) in self.store.workspaces()? {
            self.catch_up_workspace(w.id).await?;
        }
        self.run_backfills().await
    }

    // ---- backfill ----

    pub async fn run_backfills(&self) -> Result<(), SyncError> {
        loop {
            let pending = self.store.backfills()?;
            if pending.is_empty() {
                return Ok(());
            }
            for b in pending {
                self.backfill_page(&b).await?;
            }
        }
    }

    /// Fetches one page for a pending backfill. Returns true if the page was applied.
    pub async fn backfill_page(&self, b: &Backfill) -> Result<bool, SyncError> {
        let mut query = vec![
            ("until_seq", b.until_seq.to_string()),
            ("interval_seq", b.interval_seq.to_string()),
            ("limit", BACKFILL_PAGE.to_string()),
        ];
        if let Some(before) = b.before_seq {
            query.push(("before_seq", before.to_string()));
        }
        let page: MessagePage = match self
            .api(
                Method::Get,
                &format!("/conversations/{}/messages", b.conversation_id),
                &query,
                None::<&()>,
                None,
            )
            .await
        {
            Ok(p) => p,
            Err(SyncError::Transport(e)) if e.status() == Some(403) => {
                // Access ended; the stream's member_removed purges the conversation.
                return Err(SyncError::Transport(e));
            }
            Err(e) => return Err(e),
        };
        self.apply_backfill_page(b.conversation_id, page)
    }

    /// Applies a page only if its interval is still pending. A page that arrives after the
    /// interval closed is discarded, and the monotonic merge stops a late page from rolling
    /// back an edit or bringing back a deleted message.
    pub fn apply_backfill_page(
        &self,
        conversation_id: Uuid,
        page: MessagePage,
    ) -> Result<bool, SyncError> {
        let mut ops = vec![Op::RequireBackfill {
            conversation_id,
            interval_seq: page.interval_seq,
        }];
        ops.extend(page.messages.into_iter().map(Op::MergeMessage));
        ops.push(Op::AdvanceBackfill {
            conversation_id,
            interval_seq: page.interval_seq,
            before_seq: page.next_before_seq,
        });
        let changes = self.commit(ops)?;
        #[cfg(feature = "testing")]
        if !changes.aborted {
            let left = self
                .faults
                .crash_after_backfill_pages
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if left == 1 {
                return Err(SyncError::Other(anyhow::anyhow!(
                    "injected crash mid-backfill"
                )));
            }
        }
        Ok(!changes.aborted)
    }

    // ---- socket ----

    fn cursors(&self) -> anyhow::Result<Vec<StreamCursor>> {
        Ok(self
            .store
            .workspaces()?
            .into_iter()
            .map(|(w, since)| StreamCursor {
                workspace_id: w.id,
                since,
            })
            .collect())
    }

    async fn handle_frame(&self, frame: ServerFrame) -> Result<bool, SyncError> {
        match frame {
            ServerFrame::Workspace {
                workspace_id,
                events,
                next_since,
            } => {
                self.apply_workspace_page(workspace_id, &events, next_since)?;
            }
            ServerFrame::User { events, next_since } => {
                for w in self.apply_user_page(&events, next_since)? {
                    self.fetch_members(w).await?;
                }
            }
            ServerFrame::Resync { workspace_id } => {
                match workspace_id {
                    Some(w) => self.catch_up_workspace(w).await?,
                    None => self.catch_up_user().await?,
                }
                // Reconnect so the socket resumes from the new cursors.
                return Ok(false);
            }
            ServerFrame::Presence { user_id, online } => {
                let list = match self.online.lock() {
                    Ok(mut set) => {
                        if online {
                            set.insert(user_id);
                        } else {
                            set.remove(&user_id);
                        }
                        set.iter().copied().collect()
                    }
                    Err(_) => vec![],
                };
                (self.events)(self.id, AccountEvent::Presence { online: list });
            }
            ServerFrame::Typing {
                conversation_id,
                user_id,
                ..
            } => {
                if let Ok(mut t) = self.typing.lock() {
                    t.entry(conversation_id)
                        .or_default()
                        .insert(user_id, Instant::now());
                }
                self.emit_typing(conversation_id);
            }
        }
        Ok(true)
    }

    fn emit_typing(&self, conversation_id: Uuid) {
        let ids: Vec<Uuid> = match self.typing.lock() {
            Ok(mut t) => {
                let entry = t.entry(conversation_id).or_default();
                entry.retain(|_, at| at.elapsed() < Duration::from_secs(5));
                entry.keys().copied().collect()
            }
            Err(_) => vec![],
        };
        let names = self
            .names_for(conversation_id)
            .unwrap_or(Names(HashMap::new()));
        (self.events)(
            self.id,
            AccountEvent::Typing {
                conversation_id,
                names: ids.into_iter().map(|u| names.get(u)).collect(),
            },
        );
    }

    async fn run_socket(&self) -> Result<(), SyncError> {
        let token = self.token()?;
        let mut socket = self
            .transport
            .socket(&token)
            .await
            .map_err(SyncError::from_transport)?;
        let hello = ClientFrame::Hello {
            user_since: self.store.user_since()?,
            workspaces: self.cursors()?,
        };
        socket
            .send(serde_json::to_string(&hello).map_err(anyhow::Error::from)?)
            .await?;
        let (tx, mut rx) = mpsc::channel::<String>(32);
        if let Ok(mut s) = self.socket_tx.lock() {
            *s = Some(tx);
        }
        self.set_connection(ConnectionState::Online);
        let result = loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break Ok(()),
                out = rx.recv() => {
                    if let Some(text) = out
                        && let Err(e) = socket.send(text).await {
                            break Err(e.into());
                        }
                }
                frame = socket.recv() => match frame {
                    None => break Ok(()),
                    Some(Err(e)) => break Err(e.into()),
                    Some(Ok(text)) => {
                        let Ok(frame) = serde_json::from_str::<ServerFrame>(&text) else { continue };
                        match self.handle_frame(frame).await {
                            Ok(true) => {}
                            Ok(false) => break Ok(()),
                            Err(e) => break Err(e),
                        }
                    }
                }
            }
        };
        if let Ok(mut s) = self.socket_tx.lock() {
            *s = None;
        }
        result
    }

    /// Runs sync until cancelled: HTTPS catch-up, then the socket, reconnecting with backoff.
    async fn run_sync(self) {
        let mut backoff = Duration::from_millis(500);
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            self.set_connection(ConnectionState::Connecting);
            let result = async {
                self.check_version().await?;
                self.catch_up_user().await?;
                for (w, _) in self.store.workspaces()? {
                    self.catch_up_workspace(w.id).await?;
                }
                self.wake_backfill.notify_one();
                self.wake_outbox.notify_one();
                self.run_socket().await
            }
            .await;
            match result {
                Ok(()) => backoff = Duration::from_millis(500),
                Err(SyncError::SignedOut) => {
                    self.set_connection(ConnectionState::SignedOut);
                    return;
                }
                Err(e) => {
                    tracing::info!(account = %self.id, error = %e, "sync interrupted");
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
            self.set_connection(ConnectionState::Offline);
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    }

    async fn run_backfill_loop(self) {
        loop {
            if let Err(e) = self.run_backfills().await {
                tracing::debug!(error = %e, "backfill paused");
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake_backfill.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
    }

    async fn run_flush_loop(self) {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake_flush.notified() => {}
            }
            // Coalesce a burst of changes into one batch per frame.
            tokio::time::sleep(Duration::from_millis(16)).await;
            self.windows.flush(&*self.0);
        }
    }

    async fn run_outbox_loop(self) {
        loop {
            let next = self
                .process_outbox()
                .await
                .unwrap_or(Some(Duration::from_secs(5)));
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake_outbox.notified() => {}
                _ = tokio::time::sleep(next.unwrap_or(Duration::from_secs(60))) => {}
            }
        }
    }

    /// Starts the account's background tasks.
    pub fn start(&self) {
        tokio::spawn(self.clone().run_sync());
        tokio::spawn(self.clone().run_backfill_loop());
        tokio::spawn(self.clone().run_flush_loop());
        tokio::spawn(self.clone().run_outbox_loop());
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    // ---- outbox ----

    /// Delivers due outbox items in order. Returns how long until the next retry is due.
    pub async fn process_outbox(&self) -> Result<Option<Duration>, SyncError> {
        let mut next: Option<Duration> = None;
        let mut blocked: HashSet<Uuid> = HashSet::new();
        for item in self.store.outbox()? {
            if item.state == OutboxState::Failed || blocked.contains(&item.conversation_id) {
                continue;
            }
            let now = now_ms();
            if now - item.created_at_ms > OUTBOX_GIVE_UP.as_millis() as i64 {
                self.give_up(item, "not delivered after 7 days")?;
                continue;
            }
            if item.next_at_ms > now {
                let wait = Duration::from_millis((item.next_at_ms - now) as u64);
                next = Some(next.map_or(wait, |n| n.min(wait)));
                blocked.insert(item.conversation_id);
                continue;
            }
            match self.deliver(&item).await {
                Ok(message) => {
                    let mut ops = vec![Op::OutboxRemove(item.id)];
                    ops.extend(message.map(Op::MergeMessage));
                    self.commit(ops)?;
                }
                Err(SyncError::SignedOut) => return Err(SyncError::SignedOut),
                Err(SyncError::Transport(e)) if !e.is_transient() => {
                    self.rejected(item, &e)?;
                }
                Err(e) => {
                    // Keep order: later items in this conversation wait for this one.
                    blocked.insert(item.conversation_id);
                    let attempts = item.attempts + 1;
                    let delay = Duration::from_millis((500u64 << attempts.min(7)).min(60_000));
                    next = Some(next.map_or(delay, |n| n.min(delay)));
                    let mut retry = item;
                    retry.attempts = attempts;
                    retry.next_at_ms = now_ms() + delay.as_millis() as i64;
                    retry.last_error = Some(e.to_string());
                    self.commit(vec![Op::OutboxPut(retry)])?;
                }
            }
        }
        Ok(next)
    }

    async fn deliver(&self, item: &OutboxItem) -> Result<Option<Message>, SyncError> {
        match &item.kind {
            OutboxKind::Send {
                body,
                entities,
                thread_root_id,
                attachments,
            } => {
                let req = proto::api::SendMessage {
                    client_id: item.id,
                    body: body.clone(),
                    entities: entities.clone(),
                    thread_root_id: *thread_root_id,
                    attachments: attachments.clone(),
                };
                let m: Message = self
                    .api(
                        Method::Post,
                        &format!("/conversations/{}/messages", item.conversation_id),
                        &[],
                        Some(&req),
                        Some(item.id),
                    )
                    .await?;
                Ok(Some(m))
            }
            other => crate::account::deliver_other(self, item, other).await,
        }
    }

    fn give_up(&self, mut item: OutboxItem, why: &str) -> anyhow::Result<()> {
        item.state = OutboxState::Failed;
        item.last_error = Some(why.into());
        self.commit(vec![Op::OutboxPut(item)])?;
        Ok(())
    }

    /// The server refused the command for good.
    fn rejected(&self, item: OutboxItem, e: &TransportError) -> anyhow::Result<()> {
        let message = match e {
            TransportError::Status { body, .. } => body
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("rejected")
                .to_string(),
            other => other.to_string(),
        };
        match &item.kind {
            OutboxKind::Send { .. } => self.give_up(item, &message),
            OutboxKind::Edit {
                message_id, body, ..
            } => {
                let reason = if e.status() == Some(410) {
                    DraftReason::Deleted
                } else {
                    DraftReason::StaleEdit
                };
                let mut ops = vec![
                    Op::OutboxRemove(item.id),
                    Op::DraftPut(DraftView {
                        id: item.id,
                        account_id: self.id,
                        conversation_id: item.conversation_id,
                        message_id: Some(*message_id),
                        body: body.clone(),
                        reason,
                    }),
                ];
                if let TransportError::Status { body, .. } = e
                    && let Some(current) = body
                        .get("current")
                        .and_then(|c| serde_json::from_value::<Message>(c.clone()).ok())
                {
                    ops.push(Op::MergeMessage(current));
                }
                self.commit(ops)?;
                Ok(())
            }
            _ => {
                tracing::info!(error = %message, "outbox item rejected");
                self.commit(vec![Op::OutboxRemove(item.id)])?;
                Ok(())
            }
        }
    }

    // ---- commands ----

    pub fn queue(&self, conversation_id: Uuid, kind: OutboxKind) -> anyhow::Result<Uuid> {
        let id = Uuid::now_v7();
        self.commit(vec![Op::OutboxPut(OutboxItem {
            id,
            conversation_id,
            kind,
            created_at_ms: now_ms(),
            attempts: 0,
            next_at_ms: 0,
            state: OutboxState::Pending,
            last_error: None,
        })])?;
        self.wake_outbox.notify_one();
        Ok(id)
    }

    pub fn send_message(
        &self,
        conversation_id: Uuid,
        body: String,
        thread_root_id: Option<Uuid>,
        attachments: Vec<Uuid>,
    ) -> anyhow::Result<Uuid> {
        let entities = mentions(&*self.store, conversation_id, &body)?;
        self.queue(
            conversation_id,
            OutboxKind::Send {
                body,
                entities,
                thread_root_id,
                attachments,
            },
        )
    }

    pub fn edit_message(
        &self,
        conversation_id: Uuid,
        message_id: Uuid,
        base_revision: i64,
        body: String,
    ) -> anyhow::Result<Uuid> {
        let entities = mentions(&*self.store, conversation_id, &body)?;
        self.queue(
            conversation_id,
            OutboxKind::Edit {
                message_id,
                base_revision,
                body,
                entities,
            },
        )
    }

    pub fn delete_message(&self, conversation_id: Uuid, message_id: Uuid) -> anyhow::Result<Uuid> {
        self.queue(conversation_id, OutboxKind::Delete { message_id })
    }

    /// Adds the reaction, or removes it if this user already reacted with it.
    pub fn toggle_reaction(
        &self,
        conversation_id: Uuid,
        message_id: Uuid,
        emoji: String,
    ) -> anyhow::Result<Uuid> {
        let mine = self.store.message(message_id)?.is_some_and(|m| {
            m.reactions
                .iter()
                .any(|r| r.emoji == emoji && r.user_ids.contains(&self.user.id))
        });
        self.queue(
            conversation_id,
            OutboxKind::React {
                message_id,
                emoji,
                add: !mine,
            },
        )
    }

    /// Marks a conversation read up to its newest cached message, here and on the server.
    pub async fn mark_read(&self, conversation_id: Uuid) -> Result<(), SyncError> {
        let seq = self.store.latest_seq(conversation_id)?;
        if seq <= self.store.read_cursor(conversation_id)? {
            return Ok(());
        }
        self.commit(vec![Op::SetReadCursor {
            conversation_id,
            seq,
        }])?;
        let _: serde_json::Value = self
            .api(
                Method::Post,
                &format!("/conversations/{conversation_id}/read"),
                &[],
                Some(&proto::api::MarkRead { seq }),
                None,
            )
            .await?;
        Ok(())
    }

    pub fn retry(&self, id: Uuid) -> anyhow::Result<()> {
        if let Some(mut item) = self.store.outbox()?.into_iter().find(|o| o.id == id) {
            item.state = OutboxState::Pending;
            item.attempts = 0;
            item.next_at_ms = 0;
            item.created_at_ms = now_ms();
            self.commit(vec![Op::OutboxPut(item)])?;
            self.wake_outbox.notify_one();
        }
        Ok(())
    }

    pub fn discard(&self, id: Uuid) -> anyhow::Result<()> {
        self.commit(vec![Op::OutboxRemove(id), Op::DraftRemove(id)])?;
        Ok(())
    }

    pub fn set_focus(&self, conversation_id: Option<Uuid>) {
        if let Ok(mut f) = self.focus.lock() {
            *f = conversation_id;
        }
    }

    pub fn focused(&self) -> Option<Uuid> {
        self.focus.lock().ok().and_then(|f| *f)
    }

    pub fn typing(&self, conversation_id: Uuid) {
        let frame = ClientFrame::Typing { conversation_id };
        if let (Ok(Some(tx)), Ok(text)) = (
            self.socket_tx.lock().map(|s| s.clone()),
            serde_json::to_string(&frame),
        ) {
            let _ = tx.try_send(text);
        }
    }

    pub fn subscribe_window(&self, spec: WindowSpec, sink: Sink) -> u64 {
        self.windows.subscribe(spec, sink, &*self.0)
    }

    pub fn drafts(&self) -> anyhow::Result<Vec<DraftView>> {
        self.store.drafts()
    }

    pub fn notifier(&self) -> &Arc<dyn Notifier> {
        &self.notifier
    }

    // ---- search ----

    /// Searches a workspace on the server, which filters by what this user can read now.
    pub async fn search(
        &self,
        workspace_id: Uuid,
        query: &str,
    ) -> Result<Vec<proto::view::SearchHitView>, SyncError> {
        let r: proto::api::SearchResults = self
            .api(
                Method::Get,
                &format!("/workspaces/{workspace_id}/search"),
                &[("q", query.to_string())],
                None::<&()>,
                None,
            )
            .await?;
        let names: HashMap<Uuid, String> = self
            .store
            .members(workspace_id)?
            .into_iter()
            .map(|m| (m.user_id, m.display_name))
            .collect();
        Ok(r.hits
            .iter()
            .map(|h| views::search_hit_view(&*self.store, self.user.id, &names, h))
            .collect::<anyhow::Result<_>>()?)
    }

    // ---- files ----

    /// Streams a local file to a conversation. Uploads go straight to the server, not
    /// through the outbox, so they need a connection.
    pub async fn upload_file(
        &self,
        conversation_id: Uuid,
        path: &std::path::Path,
    ) -> Result<proto::model::FileInfo, SyncError> {
        let token = self.token()?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow::anyhow!("not a file: {}", path.display()))?;
        let v = self
            .transport
            .upload(
                &format!("/conversations/{conversation_id}/files"),
                &[("name", name)],
                path,
                &token,
            )
            .await
            .map_err(SyncError::from_transport)?;
        let r: proto::api::UploadResponse =
            serde_json::from_value(v).map_err(|e| anyhow::anyhow!("decode upload: {e}"))?;
        Ok(r.file)
    }

    /// Uploads a file and queues a message carrying it.
    pub async fn attach_file(
        &self,
        conversation_id: Uuid,
        thread_root_id: Option<Uuid>,
        path: &std::path::Path,
    ) -> Result<Uuid, SyncError> {
        let file = self.upload_file(conversation_id, path).await?;
        Ok(self.send_message(
            conversation_id,
            String::new(),
            thread_root_id,
            vec![file.id],
        )?)
    }

    /// A file's bytes and content type, fetched with this account's session. The server
    /// checks access on every call, so this fails once the user can no longer read it.
    pub async fn fetch_file(
        &self,
        file_id: Uuid,
        thumbnail: bool,
    ) -> Result<(Vec<u8>, String), SyncError> {
        let token = self.token()?;
        let path = if thumbnail {
            format!("/files/{file_id}/thumbnail")
        } else {
            format!("/files/{file_id}")
        };
        self.transport
            .download(&path, &token)
            .await
            .map_err(SyncError::from_transport)
    }
}

impl Inner {
    pub fn names_for(&self, conversation_id: Uuid) -> anyhow::Result<Names> {
        let ws = self
            .store
            .conversation(conversation_id)?
            .map(|(c, _)| c.workspace_id);
        let mut map = HashMap::new();
        if let Some(ws) = ws {
            for m in self.store.members(ws)? {
                map.insert(m.user_id, m.display_name);
            }
        }
        map.entry(self.user.id)
            .or_insert_with(|| self.user.display_name.clone());
        Ok(Names(map))
    }
}

impl RowSource for Inner {
    fn rows(&self, spec: &WindowSpec) -> anyhow::Result<(u32, Vec<MessageView>)> {
        let (total, items) = self.store.window(
            spec.conversation_id,
            spec.thread_root_id,
            spec.offset_from_end,
            spec.limit,
        )?;
        let names = self.names_for(spec.conversation_id)?;
        let rows = views::window_views(self.id, self.user.id, &*self.store, items, &names)?;
        Ok((total, rows))
    }
}

/// Delivers edits, deletes and reactions. Edits carry their outbox id as the idempotency
/// key, so a retried edit that already landed is not mistaken for a stale one.
async fn deliver_other(
    account: &Account,
    item: &OutboxItem,
    kind: &OutboxKind,
) -> Result<Option<Message>, SyncError> {
    match kind {
        OutboxKind::Edit {
            message_id,
            base_revision,
            body,
            entities,
        } => {
            let req = proto::api::EditMessage {
                body: body.clone(),
                entities: entities.clone(),
                base_revision: *base_revision,
            };
            let m: Message = account
                .api(
                    Method::Patch,
                    &format!("/messages/{message_id}"),
                    &[],
                    Some(&req),
                    Some(item.id),
                )
                .await?;
            Ok(Some(m))
        }
        OutboxKind::Delete { message_id } => {
            let m: Message = account
                .api(
                    Method::Delete,
                    &format!("/messages/{message_id}"),
                    &[],
                    None::<&()>,
                    Some(item.id),
                )
                .await?;
            Ok(Some(m))
        }
        OutboxKind::React {
            message_id,
            emoji,
            add,
        } => {
            let method = if *add { Method::Put } else { Method::Delete };
            let path = format!("/messages/{message_id}/reactions/{}", encode_segment(emoji));
            let _: serde_json::Value = account.api(method, &path, &[], None::<&()>, None).await?;
            Ok(None)
        }
        OutboxKind::Send { .. } => unreachable!("sends are delivered by Account::deliver"),
    }
}

fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Turns `@Display Name` into mention entities for members of the conversation's workspace.
/// Longer names win, so "@Ann Lee" is not read as "@Ann".
pub fn mentions(
    store: &dyn Storage,
    conversation_id: Uuid,
    body: &str,
) -> anyhow::Result<Vec<proto::model::Entity>> {
    let Some((conv, _)) = store.conversation(conversation_id)? else {
        return Ok(vec![]);
    };
    let mut members = store.members(conv.workspace_id)?;
    members.sort_by_key(|m| std::cmp::Reverse(m.display_name.len()));
    let mut taken = vec![false; body.len()];
    let mut out = vec![];
    for m in members {
        let needle = format!("@{}", m.display_name);
        for (start, _) in body.match_indices(&needle) {
            let end = start + needle.len();
            let boundary = body[end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
            if !boundary || taken[start..end].iter().any(|t| *t) {
                continue;
            }
            taken[start..end].iter_mut().for_each(|t| *t = true);
            out.push(proto::model::Entity::Mention {
                user_id: m.user_id,
                start: start as u32,
                end: end as u32,
            });
        }
    }
    out.sort_by_key(|e| match e {
        proto::model::Entity::Mention { start, .. } => *start,
        _ => 0,
    });
    Ok(out)
}

/// The core's presentation rule: notifications for the conversation in focus are not shown.
/// The OS applies its own do-not-disturb (Focus) setting to what is shown.
pub fn should_present(focused: Option<Uuid>, n: &Notification) -> bool {
    focused != Some(n.conversation_id)
}

fn present_notification(account: &Account, n: &Notification) {
    (account.events)(account.id, AccountEvent::Notification(n.clone()));
    if !should_present(account.focused(), n) {
        return;
    }
    let title = match n.reason {
        proto::model::NotifyReason::DirectMessage => n.author_name.clone(),
        _ => {
            let channel = account
                .store
                .conversation(n.conversation_id)
                .ok()
                .flatten()
                .and_then(|(c, _)| c.name)
                .map(|c| format!(" in #{c}"))
                .unwrap_or_default();
            format!("{}{channel}", n.author_name)
        }
    };
    account.notifier().show(account.id, &title, &n.preview, n);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use proto::model::{Conversation, ConversationKind, Member, NotifyReason, Role, Workspace};

    fn n(conv: Uuid) -> Notification {
        Notification {
            message_id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            conversation_id: conv,
            author_id: Uuid::nil(),
            author_name: "a".into(),
            reason: NotifyReason::Mention,
            preview: "p".into(),
        }
    }

    #[test]
    fn focused_conversation_is_not_shown() {
        let c = Uuid::from_u128(1);
        assert!(!should_present(Some(c), &n(c)));
        assert!(should_present(Some(Uuid::from_u128(2)), &n(c)));
        assert!(should_present(None, &n(c)));
    }

    #[test]
    fn mentions_prefer_the_longest_name() {
        let store = SqliteStore::in_memory().unwrap();
        let ws = Uuid::from_u128(9);
        let conv = Conversation {
            id: Uuid::from_u128(8),
            workspace_id: ws,
            kind: ConversationKind::Public,
            name: Some("g".into()),
            dm_user_ids: vec![],
        };
        let ann = Uuid::from_u128(1);
        let ann_lee = Uuid::from_u128(2);
        store
            .commit(vec![
                Op::AddWorkspace {
                    workspace: Workspace {
                        id: ws,
                        name: "w".into(),
                    },
                    since: 0,
                },
                Op::GrantAccess {
                    conversation: conv.clone(),
                    access_seq: 1,
                },
                Op::UpsertMember(Member {
                    user_id: ann,
                    workspace_id: ws,
                    display_name: "Ann".into(),
                    role: Role::Member,
                }),
                Op::UpsertMember(Member {
                    user_id: ann_lee,
                    workspace_id: ws,
                    display_name: "Ann Lee".into(),
                    role: Role::Member,
                }),
            ])
            .unwrap();
        let e = mentions(&store, conv.id, "hi @Ann Lee and @Ann, not @Annie").unwrap();
        let ids: Vec<Uuid> = e
            .iter()
            .map(|e| match e {
                proto::model::Entity::Mention { user_id, .. } => *user_id,
                _ => Uuid::nil(),
            })
            .collect();
        assert_eq!(ids, vec![ann_lee, ann]);
    }
}
