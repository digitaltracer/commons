//! One WebSocket per client. It only delivers; HTTPS replay is the source of truth.
use crate::sqlx;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use proto::events::{ClientFrame, ServerFrame, UserEventBody};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::hub::{self, Ephemeral, Wakeup};
use crate::{AppState, conversations, sync};

const PAGE: i64 = 200;
/// Queued feed batches filtered together, at most.
const CHAIN: usize = 8;

pub async fn upgrade(
    State(state): State<AppState>,
    auth: AuthUser,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            run(state, auth, socket).await;
        })
}

enum Stop {
    /// The client is gone or too slow; it will resume from its cursors.
    Disconnect,
}

struct Session {
    state: AppState,
    user_id: Uuid,
    sink: SplitSink<WebSocket, WsMessage>,
    workspaces: HashMap<Uuid, i64>,
    /// Access as of each workspace cursor, once the socket has caught up with the feed.
    access: HashMap<Uuid, crate::feed::Access>,
    user_since: i64,
}

impl Session {
    async fn send(&mut self, frame: &ServerFrame) -> Result<(), Stop> {
        let text = serde_json::to_string(frame).map_err(|_| Stop::Disconnect)?;
        self.send_text(text).await
    }

    async fn send_text(&mut self, text: String) -> Result<(), Stop> {
        match tokio::time::timeout(
            self.state.config.send_timeout,
            self.sink.send(WsMessage::Text(text.into())),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::Disconnect),
            Err(_) => {
                tracing::info!(user = %self.user_id, "socket write timed out; disconnecting");
                self.state.hub.dropped_slow.fetch_add(1, Ordering::Relaxed);
                Err(Stop::Disconnect)
            }
        }
    }

    /// Delivers everything after the cursor, a bounded page at a time.
    async fn drain_workspace(&mut self, workspace_id: Uuid) -> Result<(), Stop> {
        loop {
            let Some(&since) = self.workspaces.get(&workspace_id) else {
                return Ok(());
            };
            let page = async {
                let mut tx = self.state.pool.begin().await?;
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                    .execute(&mut *tx)
                    .await?;
                let p = sync::read_workspace_page(
                    &mut tx,
                    workspace_id,
                    self.user_id,
                    Some(since),
                    PAGE,
                )
                .await?;
                // Caught up: take the access state in the same snapshot, for the feed.
                let access = if p.snapshot.is_none() && p.next_since >= p.head {
                    Some(crate::feed::load_access(&mut tx, workspace_id, self.user_id).await?)
                } else {
                    None
                };
                tx.commit().await?;
                Ok::<_, ApiError>((p, access))
            }
            .await;
            let page = page.map(|(p, access)| {
                match access {
                    Some(a) => self.access.insert(workspace_id, a),
                    None => self.access.remove(&workspace_id),
                };
                p
            });
            match page {
                Ok(p) if p.snapshot.is_some() => {
                    self.workspaces.remove(&workspace_id);
                    return self
                        .send(&ServerFrame::Resync {
                            workspace_id: Some(workspace_id),
                        })
                        .await;
                }
                Ok(p) => {
                    if p.next_since == since {
                        return Ok(());
                    }
                    let caught_up = p.next_since >= p.head;
                    self.send(&ServerFrame::Workspace {
                        workspace_id,
                        events: p.events,
                        next_since: p.next_since,
                    })
                    .await?;
                    self.workspaces.insert(workspace_id, p.next_since);
                    if caught_up {
                        return Ok(());
                    }
                }
                Err(ApiError::Forbidden) => {
                    self.workspaces.remove(&workspace_id);
                    return Ok(());
                }
                Err(ApiError::Gone(_)) => {
                    self.workspaces.remove(&workspace_id);
                    return self
                        .send(&ServerFrame::Resync {
                            workspace_id: Some(workspace_id),
                        })
                        .await;
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "socket replay failed");
                    return Err(Stop::Disconnect);
                }
            }
        }
    }

    /// Delivers a feed batch: filtered in memory when it continues this socket's cursor,
    /// otherwise the socket catches up through its own replay. Batches for the same workspace
    /// already queued behind it are filtered with it; the first other wakeup is kept in
    /// `pending` so wakeups stay in order.
    async fn batch(
        &mut self,
        b: std::sync::Arc<crate::feed::Batch>,
        rx: &mut tokio::sync::mpsc::Receiver<Wakeup>,
        pending: &mut Option<Wakeup>,
    ) -> Result<(), Stop> {
        let Some(&cursor) = self.workspaces.get(&b.workspace_id) else {
            return Ok(());
        };
        if b.to <= cursor {
            return Ok(());
        }
        let Some(access) = self
            .access
            .get(&b.workspace_id)
            .filter(|_| b.from == cursor)
        else {
            self.state
                .hub
                .batches_replayed
                .fetch_add(1, Ordering::Relaxed);
            return self.drain_workspace(b.workspace_id).await;
        };
        let mut chain = vec![b];
        while chain.len() < CHAIN && pending.is_none() {
            match rx.try_recv() {
                Ok(Wakeup::Batch(next))
                    if next.workspace_id == chain[0].workspace_id
                        && chain.last().is_some_and(|l| next.from == l.to) =>
                {
                    chain.push(next)
                }
                Ok(other) => *pending = Some(other),
                Err(_) => break,
            }
        }
        self.state
            .hub
            .batches_filtered
            .fetch_add(chain.len() as u64, Ordering::Relaxed);
        let workspace_id = chain[0].workspace_id;
        let to = chain.last().map_or(cursor, |l| l.to);
        let (events, end) = access.filter_chain(&chain, self.user_id);
        let Some(events) = events else {
            self.workspaces.remove(&workspace_id);
            self.access.remove(&workspace_id);
            return Ok(());
        };
        let frame = format!(
            r#"{{"type":"workspace","workspace_id":"{}","events":[{}],"next_since":{}}}"#,
            workspace_id,
            events.join(","),
            to
        );
        self.send_text(frame).await?;
        self.workspaces.insert(workspace_id, to);
        self.access.insert(workspace_id, end);
        Ok(())
    }

    async fn drain_user(&mut self) -> Result<(), Stop> {
        crate::ws::drain_user_stream(self).await
    }

    async fn drain_all(&mut self) -> Result<(), Stop> {
        self.drain_user().await?;
        let ids: Vec<Uuid> = self.workspaces.keys().copied().collect();
        for id in ids {
            self.drain_workspace(id).await?;
        }
        Ok(())
    }

    async fn ephemeral(&mut self, e: Ephemeral) -> Result<(), Stop> {
        if !self.workspaces.contains_key(&e.workspace_id) {
            return Ok(());
        }
        if let ServerFrame::Typing {
            conversation_id,
            user_id,
            ..
        } = &e.frame
        {
            if *user_id == self.user_id {
                return Ok(());
            }
            let Ok(mut conn) = self.state.pool.acquire().await else {
                return Ok(());
            };
            if !conversations::can_read_now(&mut conn, *conversation_id, self.user_id).await {
                return Ok(());
            }
        }
        self.send(&e.frame).await
    }

    async fn client_frame(&mut self, frame: ClientFrame) {
        if let ClientFrame::Typing { conversation_id } = frame {
            let Ok(mut conn) = self.state.pool.acquire().await else {
                return;
            };
            if !conversations::can_read_now(&mut conn, conversation_id, self.user_id).await {
                return;
            }
            let Ok(workspace_id) = sqlx::query_scalar::<_, Uuid>(
                "SELECT workspace_id FROM conversations WHERE id = $1",
            )
            .bind(conversation_id)
            .fetch_one(&mut *conn)
            .await
            else {
                return;
            };
            hub::publish_ephemeral(
                &self.state.pool,
                &Ephemeral {
                    workspace_id,
                    frame: ServerFrame::Typing {
                        workspace_id,
                        conversation_id,
                        user_id: self.user_id,
                    },
                },
            )
            .await;
        }
    }
}

async fn publish_presence(state: &AppState, user_id: Uuid, workspaces: &[Uuid], online: bool) {
    for &workspace_id in workspaces {
        hub::publish_ephemeral(
            &state.pool,
            &Ephemeral {
                workspace_id,
                frame: ServerFrame::Presence { user_id, online },
            },
        )
        .await;
    }
}

async fn run(state: AppState, auth: AuthUser, socket: WebSocket) {
    let user_id = auth.user_id;
    let (sink, mut stream) = socket.split();
    let hello = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next()).await;
    let Ok(Some(Ok(WsMessage::Text(text)))) = hello else {
        return;
    };
    let Ok(ClientFrame::Hello {
        user_since,
        workspaces,
    }) = serde_json::from_str::<ClientFrame>(&text)
    else {
        return;
    };
    let mut reg = state.hub.register(user_id, auth.session);
    // A revocation that landed between the upgrade and registering found no socket to close.
    if !matches!(crate::auth::session_user(&state.pool, &auth.session).await, Ok(Some(u)) if u == user_id)
    {
        state.hub.unregister(reg.id);
        return;
    }
    let mut session = Session {
        state: state.clone(),
        user_id,
        sink,
        workspaces: workspaces
            .into_iter()
            .map(|c| (c.workspace_id, c.since))
            .collect(),
        user_since,
        access: HashMap::new(),
    };
    let ws_ids: Vec<Uuid> = session.workspaces.keys().copied().collect();
    state.hub.set_workspaces(reg.id, ws_ids.clone());
    if state.hub.connections(user_id) == 1 {
        publish_presence(&state, user_id, &ws_ids, true).await;
    }
    let mut repair = tokio::time::interval(state.config.repair_interval);
    let mut pending: Option<Wakeup> = None;
    let result: Result<(), Stop> = async {
        session.drain_all().await?;
        loop {
            let mut before: Vec<Uuid> = session.workspaces.keys().copied().collect();
            before.sort();
            let next = pending.take();
            tokio::select! {
                _ = reg.cancel.cancelled() => return Err(Stop::Disconnect),
                w = async { match next { Some(w) => Some(w), None => reg.rx.recv().await } } => match w {
                    Some(Wakeup::Batch(b)) => session.batch(b, &mut reg.rx, &mut pending).await?,
                    Some(Wakeup::User) => session.drain_user().await?,
                    Some(Wakeup::Ephemeral(e)) => session.ephemeral(e).await?,
                    Some(Wakeup::Repair) => session.drain_all().await?,
                    None => return Err(Stop::Disconnect),
                },
                msg = stream.next() => match msg {
                    Some(Ok(WsMessage::Text(t))) => {
                        if let Ok(f) = serde_json::from_str::<ClientFrame>(&t) {
                            session.client_frame(f).await;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return Ok(()),
                    Some(Ok(_)) => {}
                },
                _ = repair.tick() => session.drain_all().await?,
            }
            let mut after: Vec<Uuid> = session.workspaces.keys().copied().collect();
            after.sort();
            if after != before {
                state.hub.set_workspaces(reg.id, after);
            }
        }
    }
    .await;
    let _ = result;
    if state.hub.unregister(reg.id) {
        publish_presence(&state, user_id, &ws_ids, false).await;
    }
}

/// Delivers the user's own stream and follows workspace joins and removals.
async fn drain_user_stream(session: &mut Session) -> Result<(), Stop> {
    loop {
        let since = session.user_since;
        let page = async {
            let mut tx = session.state.pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            let p = sync::read_user_page(&mut tx, session.user_id, Some(since), PAGE).await?;
            tx.commit().await?;
            Ok::<_, ApiError>(p)
        }
        .await;
        let page = match page {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = ?e, "user replay failed");
                return Err(Stop::Disconnect);
            }
        };
        if page.snapshot.is_some() {
            session
                .send(&ServerFrame::Resync { workspace_id: None })
                .await?;
            return Err(Stop::Disconnect);
        }
        if page.next_since == since {
            return Ok(());
        }
        let mut joined = vec![];
        for e in &page.events {
            match &e.body {
                UserEventBody::WorkspaceJoined {
                    workspace,
                    start_seq,
                } => {
                    session
                        .workspaces
                        .entry(workspace.id)
                        .or_insert((start_seq - 1).max(0));
                    joined.push(workspace.id);
                }
                UserEventBody::WorkspaceRemoved { workspace_id } => {
                    session.workspaces.remove(workspace_id);
                    session.access.remove(workspace_id);
                }
                _ => {}
            }
        }
        let caught_up = page.next_since >= page.head;
        session
            .send(&ServerFrame::User {
                events: page.events,
                next_since: page.next_since,
            })
            .await?;
        session.user_since = page.next_since;
        for w in joined {
            session.drain_workspace(w).await?;
        }
        if caught_up {
            return Ok(());
        }
    }
}
