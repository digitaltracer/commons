//! M0 webview spike (step 1): three local accounts with 20 workspaces between them, one
//! conversation of 10,000 messages, and bursts of live inserts and edits. No server.
//! The UI measures batch-to-paint and scroll frame times; `cargo xtask spike` samples RSS.
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use client_core::Core;
use client_core::account::{Account, AccountConfig};
use client_core::platform::RecordingNotifier;
use client_core::store::{Op, SqliteStore};
use client_core::transport::{Method, Socket, Transport, TransportError};
use proto::model::{Conversation, ConversationKind, Member, Message, Role, User, Workspace};
use serde::Serialize;
use tauri::{AppHandle, State};
use uuid::Uuid;

use crate::Host;

pub const MESSAGES: usize = 10_000;
pub const LIVE_SECONDS: u32 = 20;

pub struct Spike {
    account_id: Uuid,
    conversation_id: Uuid,
    out: Option<String>,
    reports: Mutex<serde_json::Map<String, serde_json::Value>>,
    app: AppHandle,
}

/// A transport with no server behind it; the spike's accounts stay offline.
struct Offline;

#[async_trait::async_trait]
impl Transport for Offline {
    async fn request(
        &self,
        _: Method,
        _: &str,
        _: &[(&str, String)],
        _: Option<serde_json::Value>,
        _: Option<&str>,
        _: Option<Uuid>,
    ) -> Result<serde_json::Value, TransportError> {
        Err(TransportError::Network("spike: offline".into()))
    }
    async fn socket(&self, _: &str) -> Result<Box<dyn Socket>, TransportError> {
        Err(TransportError::Network("spike: offline".into()))
    }
    async fn upload(
        &self,
        _: &str,
        _: &[(&str, String)],
        _: &Path,
        _: &str,
        _: client_core::transport::Progress,
    ) -> Result<serde_json::Value, TransportError> {
        Err(TransportError::Network("spike: offline".into()))
    }
    async fn download(&self, _: &str, _: &str) -> Result<(Vec<u8>, String), TransportError> {
        Err(TransportError::Network("spike: offline".into()))
    }
}

const WORDS: &[&str] = &[
    "deploy",
    "review",
    "lunch",
    "the",
    "build",
    "is",
    "green",
    "again",
    "ship",
    "it",
    "today",
    "please",
    "look",
    "at",
    "this",
    "**bold**",
    "`code`",
    "migration",
    "standup",
    "notes",
];

fn body(n: usize) -> String {
    let len = 4 + (n * 7919) % 30;
    (0..len)
        .map(|i| WORDS[(n * 31 + i * 17) % WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

fn message(ws: Uuid, conv: Uuid, author: Uuid, seq: i64, text: String) -> Message {
    Message {
        id: Uuid::now_v7(),
        workspace_id: ws,
        conversation_id: conv,
        author_id: author,
        client_id: Uuid::now_v7(),
        thread_root_id: None,
        body: text,
        entities: vec![],
        content_type: "text/markdown".into(),
        encryption: None,
        attachments: vec![],
        reactions: vec![],
        revision: 0,
        deleted: false,
        created_seq: seq,
        updated_seq: seq,
        created_at_ms: 1_700_000_000_000 + seq * 1000,
        edited_at_ms: None,
    }
}

pub fn start(core: &Arc<Core>, data_dir: &Path, app: AppHandle) -> anyhow::Result<Spike> {
    let mut main = None;
    let workspaces_per_account = [7, 7, 6];
    for (a, n_ws) in workspaces_per_account.iter().enumerate() {
        let id = Uuid::now_v7();
        let user = User {
            id: Uuid::now_v7(),
            email: format!("spike{a}@example.org"),
            display_name: format!("Spike {a}"),
        };
        let store = Arc::new(SqliteStore::open(
            &data_dir.join(format!("spike-{a}")).join("cache.sqlite"),
        )?);
        let account = Account::new(
            AccountConfig {
                id,
                server_url: format!("https://spike-{a}.invalid"),
                user: user.clone(),
                data_dir: data_dir.to_path_buf(),
            },
            Arc::new(Offline),
            store,
            Some("offline".into()),
            Arc::new(RecordingNotifier::default()),
            core.account_events(),
        );
        for w in 0..*n_ws {
            let ws = Workspace {
                id: Uuid::now_v7(),
                name: format!("Workspace {a}.{w}"),
            };
            let mut ops = vec![Op::AddWorkspace {
                workspace: ws.clone(),
                since: 0,
            }];
            let others: Vec<Uuid> = (0..8).map(|_| Uuid::now_v7()).collect();
            ops.push(Op::UpsertMember(Member {
                user_id: user.id,
                workspace_id: ws.id,
                display_name: user.display_name.clone(),
                role: Role::Member,
            }));
            for (i, o) in others.iter().enumerate() {
                ops.push(Op::UpsertMember(Member {
                    user_id: *o,
                    workspace_id: ws.id,
                    display_name: format!("Person {i}"),
                    role: Role::Member,
                }));
            }
            let mut convs = vec![];
            for c in 0..6 {
                let conv = Conversation {
                    id: Uuid::now_v7(),
                    workspace_id: ws.id,
                    kind: ConversationKind::Public,
                    name: Some(format!("channel-{c}")),
                    dm_user_ids: vec![],
                };
                ops.push(Op::GrantAccess {
                    conversation: conv.clone(),
                    access_seq: 1,
                });
                convs.push(conv.id);
            }
            account.store.commit(ops)?;
            let big = a == 0 && w == 0;
            let count = if big { MESSAGES } else { 200 };
            for chunk in (0..count).collect::<Vec<_>>().chunks(1000) {
                let ops = chunk
                    .iter()
                    .map(|n| {
                        let author = if n % 5 == 0 {
                            user.id
                        } else {
                            others[n % others.len()]
                        };
                        Op::MergeMessage(message(ws.id, convs[0], author, *n as i64 + 2, body(*n)))
                    })
                    .collect();
                account.store.commit(ops)?;
            }
            if big {
                main = Some((id, convs[0], ws.id, others.clone()));
            }
        }
        account.start();
        core.attach(account)?;
    }
    let (account_id, conversation_id, ws, others) =
        main.ok_or_else(|| anyhow::anyhow!("no spike conversation"))?;
    let account = core.account(account_id)?;
    tauri::async_runtime::spawn(bursts(account, ws, conversation_id, others));
    Ok(Spike {
        account_id,
        conversation_id,
        out: std::env::var("COMMONS_SPIKE_OUT").ok(),
        reports: Mutex::new(serde_json::Map::new()),
        app,
    })
}

/// Every second: 50 new messages, one commit each, plus 20 edits of recent ones.
async fn bursts(account: Account, ws: Uuid, conv: Uuid, others: Vec<Uuid>) {
    let mut seq = MESSAGES as i64 + 10;
    tokio::time::sleep(Duration::from_secs(3)).await;
    for round in 0..LIVE_SECONDS {
        for i in 0..50 {
            seq += 1;
            let m = message(ws, conv, others[i % others.len()], seq, body(seq as usize));
            let _ = account.commit(vec![Op::MergeMessage(m)]);
        }
        let recent = account.store.messages(conv).unwrap_or_default();
        for m in recent.iter().rev().take(20) {
            seq += 1;
            let mut e = m.clone();
            e.revision += 1;
            e.updated_seq = seq;
            e.body = format!("{} (edit {round})", m.body);
            e.edited_at_ms = Some(1);
            let _ = account.commit(vec![Op::MergeMessage(e)]);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpikeConfig {
    enabled: bool,
    account_id: Option<Uuid>,
    conversation_id: Option<Uuid>,
    live_seconds: u32,
}

#[tauri::command]
pub fn spike_config(host: State<'_, Host>) -> SpikeConfig {
    tracing::info!("spike: UI booted");
    match &host.spike {
        Some(s) => SpikeConfig {
            enabled: true,
            account_id: Some(s.account_id),
            conversation_id: Some(s.conversation_id),
            live_seconds: LIVE_SECONDS + 3,
        },
        None => SpikeConfig {
            enabled: false,
            account_id: None,
            conversation_id: None,
            live_seconds: 0,
        },
    }
}

#[tauri::command]
pub fn spike_report(host: State<'_, Host>, phase: String, report: serde_json::Value) {
    let Some(s) = &host.spike else { return };
    tracing::info!(%phase, "spike: report received");
    let done = phase == "scroll";
    if let Ok(mut r) = s.reports.lock() {
        r.insert(phase, report);
        if let Some(path) = &s.out {
            let _ = std::fs::write(path, serde_json::to_vec_pretty(&*r).unwrap_or_default());
        }
    }
    if done {
        let app = s.app.clone();
        std::thread::spawn(move || {
            // Leave time for a final memory sample before exiting.
            std::thread::sleep(Duration::from_secs(3));
            app.exit(0);
        });
    }
}
