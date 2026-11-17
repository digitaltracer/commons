//! One WebSocket per client. It only delivers; HTTPS replay is the source of truth.
use crate::sqlx;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use proto::events::{ClientFrame, ServerFrame};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::hub::{self, Ephemeral, Wakeup};
use crate::{AppState, conversations, sync};

const PAGE: i64 = 200;

pub async fn upgrade(
    State(state): State<AppState>,
    auth: AuthUser,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            run(state, auth.user_id, socket).await;
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
    user_since: i64,
}

impl Session {
    async fn send(&mut self, frame: &ServerFrame) -> Result<(), Stop> {
        let text = serde_json::to_string(frame).map_err(|_| Stop::Disconnect)?;
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
                tx.commit().await?;
                Ok::<_, ApiError>(p)
            }
            .await;
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

async fn run(state: AppState, user_id: Uuid, socket: WebSocket) {
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
    let mut reg = state.hub.register(user_id);
    let mut session = Session {
        state: state.clone(),
        user_id,
        sink,
        workspaces: workspaces
            .into_iter()
            .map(|c| (c.workspace_id, c.since))
            .collect(),
        user_since,
    };
    let ws_ids: Vec<Uuid> = session.workspaces.keys().copied().collect();
    state.hub.set_workspaces(reg.id, ws_ids.clone());
    if state.hub.connections(user_id) == 1 {
        publish_presence(&state, user_id, &ws_ids, true).await;
    }
    let mut repair = tokio::time::interval(state.config.repair_interval);
    let result: Result<(), Stop> = async {
        session.drain_all().await?;
        loop {
            let mut before: Vec<Uuid> = session.workspaces.keys().copied().collect();
            before.sort();
            tokio::select! {
                _ = reg.cancel.cancelled() => return Err(Stop::Disconnect),
                w = reg.rx.recv() => match w {
                    Some(Wakeup::Workspace(id)) => session.drain_workspace(id).await?,
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

/// The per-user stream arrives in step 2.
async fn drain_user_stream(_session: &mut Session) -> Result<(), Stop> {
    let _ = _session.user_since;
    Ok(())
}
