//! The offline cache, behind [`Storage`] so a browser build can use IndexedDB or OPFS.
//!
//! Core logic decides *what* changes and expresses it as [`Op`]s; storage applies a batch
//! atomically. The merge rules that keep the cache monotonic live in [`Op::MergeMessage`]:
//! a message is replaced only by a state with a larger `(revision, updated_seq)`, and a
//! tombstone is never brought back.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use proto::model::{Conversation, ConversationKind, Member, Message, Role, Workspace};
use proto::view::DraftView;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboxKind {
    Send {
        body: String,
        entities: Vec<proto::model::Entity>,
        thread_root_id: Option<Uuid>,
        attachments: Vec<Uuid>,
    },
    Edit {
        message_id: Uuid,
        base_revision: i64,
        body: String,
        entities: Vec<proto::model::Entity>,
    },
    Delete {
        message_id: Uuid,
    },
    React {
        message_id: Uuid,
        emoji: String,
        add: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxItem {
    /// Client-generated UUIDv7: the message's client id and the idempotency key.
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub kind: OutboxKind,
    pub created_at_ms: i64,
    pub attempts: u32,
    pub next_at_ms: i64,
    pub state: OutboxState,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backfill {
    pub conversation_id: Uuid,
    /// The access epoch the backfill serves; pages for another epoch are discarded.
    pub interval_seq: i64,
    pub until_seq: i64,
    pub before_seq: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    SetUserSince(i64),
    SetMeta(String, String),
    /// Adds a workspace if unknown, starting its stream after `since`.
    AddWorkspace {
        workspace: Workspace,
        since: i64,
    },
    SetWorkspaceSince(Uuid, i64),
    /// Drops a workspace and everything cached for it.
    RemoveWorkspace(Uuid),
    /// Clears a workspace's conversations, messages, members and backfills (before a snapshot).
    ResetWorkspace(Uuid),
    UpsertMember(Member),
    RemoveMember {
        workspace_id: Uuid,
        user_id: Uuid,
    },
    SetMemberRole {
        workspace_id: Uuid,
        user_id: Uuid,
        role: Role,
    },
    /// Stores conversation metadata; access stays as it was.
    UpsertConversation(Conversation),
    /// Marks the conversation readable from access epoch `access_seq`.
    GrantAccess {
        conversation: Conversation,
        access_seq: i64,
    },
    /// Access ended: drop the conversation, its messages and its backfills.
    PurgeConversation(Uuid),
    MergeMessage(Message),
    ApplyReaction {
        message_id: Uuid,
        user_id: Uuid,
        emoji: String,
        add: bool,
        seq: i64,
    },
    AddBackfill(Backfill),
    /// Aborts the whole batch unless this backfill is still pending.
    RequireBackfill {
        conversation_id: Uuid,
        interval_seq: i64,
    },
    /// Records progress; `None` means history is exhausted and the backfill is done.
    AdvanceBackfill {
        conversation_id: Uuid,
        interval_seq: i64,
        before_seq: Option<i64>,
    },
    OutboxPut(OutboxItem),
    OutboxRemove(Uuid),
    DraftPut(DraftView),
    DraftRemove(Uuid),
    /// Moves the read cursor forward; never back.
    SetReadCursor {
        conversation_id: Uuid,
        seq: i64,
    },
}

#[derive(Debug, Default, Clone)]
pub struct Changes {
    pub conversations: HashSet<Uuid>,
    pub sidebar: bool,
    pub drafts: bool,
    /// A precondition failed; nothing was written.
    pub aborted: bool,
}

pub trait Storage: Send + Sync {
    fn commit(&self, ops: Vec<Op>) -> anyhow::Result<Changes>;

    fn user_since(&self) -> anyhow::Result<i64>;
    fn meta(&self, key: &str) -> anyhow::Result<Option<String>>;
    fn workspaces(&self) -> anyhow::Result<Vec<(Workspace, i64)>>;
    fn members(&self, workspace_id: Uuid) -> anyhow::Result<Vec<Member>>;
    /// Conversations with their access epoch; `None` means not readable.
    fn conversations(&self, workspace_id: Uuid)
    -> anyhow::Result<Vec<(Conversation, Option<i64>)>>;
    fn conversation(&self, id: Uuid) -> anyhow::Result<Option<(Conversation, Option<i64>)>>;
    fn message(&self, id: Uuid) -> anyhow::Result<Option<Message>>;
    fn message_by_client_id(&self, client_id: Uuid) -> anyhow::Result<Option<Message>>;
    fn messages(&self, conversation_id: Uuid) -> anyhow::Result<Vec<Message>>;
    /// Rows for a window, oldest first, plus the total row count.
    fn window(
        &self,
        conversation_id: Uuid,
        thread_root_id: Option<Uuid>,
        offset_from_end: u32,
        limit: u32,
    ) -> anyhow::Result<(u32, Vec<WindowItem>)>;
    fn reply_counts(&self, roots: &[Uuid]) -> anyhow::Result<HashMap<Uuid, u32>>;
    fn backfills(&self) -> anyhow::Result<Vec<Backfill>>;
    fn outbox(&self) -> anyhow::Result<Vec<OutboxItem>>;
    fn drafts(&self) -> anyhow::Result<Vec<DraftView>>;
    fn read_cursor(&self, conversation_id: Uuid) -> anyhow::Result<i64>;
    /// (unread, mentions) for top-level messages by others after the read cursor.
    fn unread(&self, conversation_id: Uuid, me: Uuid) -> anyhow::Result<(u32, u32)>;
    fn latest_seq(&self, conversation_id: Uuid) -> anyhow::Result<i64>;
}

#[derive(Debug, Clone)]
pub enum WindowItem {
    Message(Message),
    Pending(OutboxItem),
}

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS workspaces (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, since INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS members (
    workspace_id TEXT NOT NULL, user_id TEXT NOT NULL, display_name TEXT NOT NULL,
    role TEXT NOT NULL, PRIMARY KEY (workspace_id, user_id));
CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, json TEXT NOT NULL, access_seq INTEGER);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY, client_id TEXT NOT NULL, conversation_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL, thread_root_id TEXT, author_id TEXT NOT NULL,
    created_seq INTEGER NOT NULL, revision INTEGER NOT NULL, updated_seq INTEGER NOT NULL,
    deleted INTEGER NOT NULL, json TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS messages_conv ON messages (conversation_id, thread_root_id, created_seq);
CREATE INDEX IF NOT EXISTS messages_client ON messages (client_id);
CREATE INDEX IF NOT EXISTS messages_root ON messages (thread_root_id);
CREATE TABLE IF NOT EXISTS backfills (
    conversation_id TEXT NOT NULL, interval_seq INTEGER NOT NULL, until_seq INTEGER NOT NULL,
    before_seq INTEGER, PRIMARY KEY (conversation_id, interval_seq));
CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS drafts (id TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS read_cursors (conversation_id TEXT PRIMARY KEY, seq INTEGER NOT NULL);
"#;

fn uuid(s: String) -> rusqlite::Result<Uuid> {
    s.parse().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn json<T: serde::de::DeserializeOwned>(s: String) -> rusqlite::Result<T> {
    serde_json::from_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn to_json<T: Serialize>(v: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string(v)?)
}

impl SqliteStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(SqliteStore {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(SqliteStore {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn purge_conversation(tx: &rusqlite::Transaction, id: &str) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM messages WHERE conversation_id = ?1", [id])?;
    tx.execute("DELETE FROM backfills WHERE conversation_id = ?1", [id])?;
    tx.execute("DELETE FROM read_cursors WHERE conversation_id = ?1", [id])?;
    tx.execute("DELETE FROM conversations WHERE id = ?1", [id])?;
    Ok(())
}

fn conversation_ids(tx: &rusqlite::Transaction, ws: &str) -> rusqlite::Result<Vec<String>> {
    let mut st = tx.prepare("SELECT id FROM conversations WHERE workspace_id = ?1")?;
    let ids = st
        .query_map([ws], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn put_message(tx: &rusqlite::Transaction, m: &Message) -> anyhow::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO messages (id, client_id, conversation_id, workspace_id,
            thread_root_id, author_id, created_seq, revision, updated_seq, deleted, json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            m.id.to_string(),
            m.client_id.to_string(),
            m.conversation_id.to_string(),
            m.workspace_id.to_string(),
            m.thread_root_id.map(|t| t.to_string()),
            m.author_id.to_string(),
            m.created_seq,
            m.revision,
            m.updated_seq,
            m.deleted,
            to_json(m)?
        ],
    )?;
    Ok(())
}

fn get_message(conn: &Connection, id: &str) -> rusqlite::Result<Option<Message>> {
    conn.query_row("SELECT json FROM messages WHERE id = ?1", [id], |r| {
        json(r.get::<_, String>(0)?)
    })
    .optional()
}

impl Storage for SqliteStore {
    fn commit(&self, ops: Vec<Op>) -> anyhow::Result<Changes> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut ch = Changes::default();
        for op in ops {
            match op {
                Op::SetUserSince(s) => {
                    tx.execute(
                        "INSERT OR REPLACE INTO meta (key, value) VALUES ('user_since', ?1)",
                        [s.to_string()],
                    )?;
                }
                Op::SetMeta(k, v) => {
                    // Preferences decide which conversations show as muted.
                    ch.sidebar |= k == "preferences";
                    tx.execute(
                        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                        [k, v],
                    )?;
                }
                Op::AddWorkspace { workspace, since } => {
                    tx.execute(
                        "INSERT INTO workspaces (id, name, since) VALUES (?1, ?2, ?3)
                         ON CONFLICT (id) DO UPDATE SET name = excluded.name",
                        params![workspace.id.to_string(), workspace.name, since],
                    )?;
                    ch.sidebar = true;
                }
                Op::SetWorkspaceSince(id, s) => {
                    tx.execute(
                        "UPDATE workspaces SET since = ?2 WHERE id = ?1",
                        params![id.to_string(), s],
                    )?;
                }
                Op::RemoveWorkspace(id) | Op::ResetWorkspace(id) => {
                    let ws = id.to_string();
                    for c in conversation_ids(&tx, &ws)? {
                        ch.conversations.insert(uuid(c.clone())?);
                        purge_conversation(&tx, &c)?;
                    }
                    tx.execute("DELETE FROM members WHERE workspace_id = ?1", [&ws])?;
                    if matches!(op, Op::RemoveWorkspace(_)) {
                        tx.execute("DELETE FROM workspaces WHERE id = ?1", [&ws])?;
                    }
                    ch.sidebar = true;
                }
                Op::UpsertMember(m) => {
                    tx.execute(
                        "INSERT OR REPLACE INTO members (workspace_id, user_id, display_name, role)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            m.workspace_id.to_string(),
                            m.user_id.to_string(),
                            m.display_name,
                            m.role.as_str()
                        ],
                    )?;
                    ch.sidebar = true;
                }
                Op::RemoveMember {
                    workspace_id,
                    user_id,
                } => {
                    tx.execute(
                        "DELETE FROM members WHERE workspace_id = ?1 AND user_id = ?2",
                        [workspace_id.to_string(), user_id.to_string()],
                    )?;
                    ch.sidebar = true;
                }
                Op::SetMemberRole {
                    workspace_id,
                    user_id,
                    role,
                } => {
                    tx.execute(
                        "UPDATE members SET role = ?3 WHERE workspace_id = ?1 AND user_id = ?2",
                        [
                            workspace_id.to_string(),
                            user_id.to_string(),
                            role.as_str().into(),
                        ],
                    )?;
                    ch.sidebar = true;
                }
                Op::UpsertConversation(c) => {
                    tx.execute(
                        "INSERT INTO conversations (id, workspace_id, json, access_seq)
                         VALUES (?1, ?2, ?3, NULL)
                         ON CONFLICT (id) DO UPDATE SET json = excluded.json",
                        params![c.id.to_string(), c.workspace_id.to_string(), to_json(&c)?],
                    )?;
                    ch.sidebar = true;
                }
                Op::GrantAccess {
                    conversation: c,
                    access_seq,
                } => {
                    tx.execute(
                        "INSERT INTO conversations (id, workspace_id, json, access_seq)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT (id) DO UPDATE SET json = excluded.json,
                                                        access_seq = excluded.access_seq",
                        params![
                            c.id.to_string(),
                            c.workspace_id.to_string(),
                            to_json(&c)?,
                            access_seq
                        ],
                    )?;
                    ch.sidebar = true;
                    ch.conversations.insert(c.id);
                }
                Op::PurgeConversation(id) => {
                    purge_conversation(&tx, &id.to_string())?;
                    ch.sidebar = true;
                    ch.conversations.insert(id);
                }
                Op::MergeMessage(m) => {
                    let readable: bool = tx
                        .query_row(
                            "SELECT access_seq IS NOT NULL FROM conversations WHERE id = ?1",
                            [m.conversation_id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if !readable {
                        continue;
                    }
                    let keep = match get_message(&tx, &m.id.to_string())? {
                        None => true,
                        Some(old) => !old.deleted && m.merge_key() > old.merge_key(),
                    };
                    if keep {
                        put_message(&tx, &m)?;
                        ch.conversations.insert(m.conversation_id);
                        ch.sidebar = true;
                    }
                }
                Op::ApplyReaction {
                    message_id,
                    user_id,
                    emoji,
                    add,
                    seq,
                } => {
                    let Some(mut m) = get_message(&tx, &message_id.to_string())? else {
                        continue;
                    };
                    if m.deleted || m.updated_seq >= seq {
                        continue;
                    }
                    m.updated_seq = seq;
                    let idx = m.reactions.iter().position(|r| r.emoji == emoji);
                    match (idx, add) {
                        (Some(i), true) => {
                            if !m.reactions[i].user_ids.contains(&user_id) {
                                m.reactions[i].user_ids.push(user_id);
                            }
                        }
                        (None, true) => m.reactions.push(proto::model::Reaction {
                            emoji,
                            user_ids: vec![user_id],
                        }),
                        (Some(i), false) => {
                            m.reactions[i].user_ids.retain(|u| *u != user_id);
                            if m.reactions[i].user_ids.is_empty() {
                                m.reactions.remove(i);
                            }
                        }
                        (None, false) => {}
                    }
                    put_message(&tx, &m)?;
                    ch.conversations.insert(m.conversation_id);
                }
                Op::AddBackfill(b) => {
                    tx.execute(
                        "INSERT OR IGNORE INTO backfills (conversation_id, interval_seq, until_seq, before_seq)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![b.conversation_id.to_string(), b.interval_seq, b.until_seq, b.before_seq],
                    )?;
                }
                Op::RequireBackfill {
                    conversation_id,
                    interval_seq,
                } => {
                    let exists: bool = tx.query_row(
                        "SELECT EXISTS (SELECT 1 FROM backfills
                          WHERE conversation_id = ?1 AND interval_seq = ?2)",
                        params![conversation_id.to_string(), interval_seq],
                        |r| r.get(0),
                    )?;
                    if !exists {
                        drop(tx);
                        return Ok(Changes {
                            aborted: true,
                            ..Changes::default()
                        });
                    }
                }
                Op::AdvanceBackfill {
                    conversation_id,
                    interval_seq,
                    before_seq,
                } => match before_seq {
                    Some(b) => {
                        tx.execute(
                            "UPDATE backfills SET before_seq = ?3
                              WHERE conversation_id = ?1 AND interval_seq = ?2",
                            params![conversation_id.to_string(), interval_seq, b],
                        )?;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM backfills WHERE conversation_id = ?1 AND interval_seq = ?2",
                            params![conversation_id.to_string(), interval_seq],
                        )?;
                    }
                },
                Op::OutboxPut(item) => {
                    tx.execute(
                        "INSERT OR REPLACE INTO outbox (id, conversation_id, json, created_at_ms)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            item.id.to_string(),
                            item.conversation_id.to_string(),
                            to_json(&item)?,
                            item.created_at_ms
                        ],
                    )?;
                    ch.conversations.insert(item.conversation_id);
                    ch.sidebar = true;
                }
                Op::OutboxRemove(id) => {
                    let conv: Option<String> = tx
                        .query_row(
                            "SELECT conversation_id FROM outbox WHERE id = ?1",
                            [id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()?;
                    tx.execute("DELETE FROM outbox WHERE id = ?1", [id.to_string()])?;
                    if let Some(c) = conv {
                        ch.conversations.insert(uuid(c)?);
                        ch.sidebar = true;
                    }
                }
                Op::DraftPut(d) => {
                    tx.execute(
                        "INSERT OR REPLACE INTO drafts (id, json) VALUES (?1, ?2)",
                        [d.id.to_string(), to_json(&d)?],
                    )?;
                    ch.drafts = true;
                }
                Op::DraftRemove(id) => {
                    tx.execute("DELETE FROM drafts WHERE id = ?1", [id.to_string()])?;
                    ch.drafts = true;
                }
                Op::SetReadCursor {
                    conversation_id,
                    seq,
                } => {
                    tx.execute(
                        "INSERT INTO read_cursors (conversation_id, seq) VALUES (?1, ?2)
                         ON CONFLICT (conversation_id) DO UPDATE
                            SET seq = max(read_cursors.seq, excluded.seq)",
                        params![conversation_id.to_string(), seq],
                    )?;
                    ch.sidebar = true;
                }
            }
        }
        tx.commit()?;
        Ok(ch)
    }

    fn user_since(&self) -> anyhow::Result<i64> {
        Ok(self
            .meta("user_since")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    fn workspaces(&self) -> anyhow::Result<Vec<(Workspace, i64)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, name, since FROM workspaces ORDER BY name")?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    Workspace {
                        id: uuid(r.get(0)?)?,
                        name: r.get(1)?,
                    },
                    r.get(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn members(&self, workspace_id: Uuid) -> anyhow::Result<Vec<Member>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT user_id, display_name, role FROM members WHERE workspace_id = ?1
             ORDER BY display_name",
        )?;
        let rows = st
            .query_map([workspace_id.to_string()], |r| {
                Ok(Member {
                    workspace_id,
                    user_id: uuid(r.get(0)?)?,
                    display_name: r.get(1)?,
                    role: Role::parse(&r.get::<_, String>(2)?).unwrap_or(Role::Member),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn conversations(
        &self,
        workspace_id: Uuid,
    ) -> anyhow::Result<Vec<(Conversation, Option<i64>)>> {
        let conn = self.lock();
        let mut st =
            conn.prepare("SELECT json, access_seq FROM conversations WHERE workspace_id = ?1")?;
        let mut rows = st
            .query_map([workspace_id.to_string()], |r| {
                Ok((json::<Conversation>(r.get(0)?)?, r.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.sort_by(|a, b| {
            let rank = |c: &Conversation| match c.kind {
                ConversationKind::Public | ConversationKind::Private => 0,
                _ => 1,
            };
            (rank(&a.0), a.0.name.clone()).cmp(&(rank(&b.0), b.0.name.clone()))
        });
        Ok(rows)
    }

    fn conversation(&self, id: Uuid) -> anyhow::Result<Option<(Conversation, Option<i64>)>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT json, access_seq FROM conversations WHERE id = ?1",
                [id.to_string()],
                |r| Ok((json(r.get(0)?)?, r.get(1)?)),
            )
            .optional()?)
    }

    fn message(&self, id: Uuid) -> anyhow::Result<Option<Message>> {
        Ok(get_message(&self.lock(), &id.to_string())?)
    }

    fn message_by_client_id(&self, client_id: Uuid) -> anyhow::Result<Option<Message>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT json FROM messages WHERE client_id = ?1",
                [client_id.to_string()],
                |r| json(r.get(0)?),
            )
            .optional()?)
    }

    fn messages(&self, conversation_id: Uuid) -> anyhow::Result<Vec<Message>> {
        let conn = self.lock();
        let mut st = conn
            .prepare("SELECT json FROM messages WHERE conversation_id = ?1 ORDER BY created_seq")?;
        let rows = st
            .query_map([conversation_id.to_string()], |r| json(r.get(0)?))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn window(
        &self,
        conversation_id: Uuid,
        thread_root_id: Option<Uuid>,
        offset_from_end: u32,
        limit: u32,
    ) -> anyhow::Result<(u32, Vec<WindowItem>)> {
        let conn = self.lock();
        let conv = conversation_id.to_string();
        let root = thread_root_id.map(|r| r.to_string());
        let pending: Vec<OutboxItem> = {
            let mut st = conn.prepare(
                "SELECT json FROM outbox o WHERE conversation_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.client_id = o.id)
                 ORDER BY created_at_ms",
            )?;
            st.query_map([&conv], |r| json::<OutboxItem>(r.get(0)?))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|o| match &o.kind {
                    OutboxKind::Send { thread_root_id, .. } => {
                        *thread_root_id == thread_root_id_of(&root)
                    }
                    _ => false,
                })
                .collect()
        };
        let filter = "conversation_id = ?1 AND deleted = 0 AND
              ((?2 IS NULL AND thread_root_id IS NULL) OR thread_root_id = ?2 OR id = ?2)";
        let n_msgs: u32 = conn.query_row(
            &format!("SELECT count(*) FROM messages WHERE {filter}"),
            params![conv, root],
            |r| r.get(0),
        )?;
        let n_pending = pending.len() as u32;
        let total = n_msgs + n_pending;
        let end = total.saturating_sub(offset_from_end);
        let start = end.saturating_sub(limit);
        let mut items = Vec::new();
        if start < n_msgs {
            let msg_end = end.min(n_msgs);
            let mut st = conn.prepare(&format!(
                "SELECT json FROM messages WHERE {filter}
                 ORDER BY created_seq LIMIT ?3 OFFSET ?4"
            ))?;
            let rows = st
                .query_map(params![conv, root, msg_end - start, start], |r| {
                    json::<Message>(r.get(0)?)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            items.extend(rows.into_iter().map(WindowItem::Message));
        }
        let p_start = start.saturating_sub(n_msgs) as usize;
        let p_end = end.saturating_sub(n_msgs) as usize;
        items.extend(
            pending
                .into_iter()
                .skip(p_start)
                .take(p_end.saturating_sub(p_start))
                .map(WindowItem::Pending),
        );
        Ok((total, items))
    }

    fn reply_counts(&self, roots: &[Uuid]) -> anyhow::Result<HashMap<Uuid, u32>> {
        let conn = self.lock();
        let mut st = conn
            .prepare("SELECT count(*) FROM messages WHERE thread_root_id = ?1 AND deleted = 0")?;
        let mut out = HashMap::new();
        for r in roots {
            let n: u32 = st.query_row([r.to_string()], |row| row.get(0))?;
            if n > 0 {
                out.insert(*r, n);
            }
        }
        Ok(out)
    }

    fn backfills(&self) -> anyhow::Result<Vec<Backfill>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT conversation_id, interval_seq, until_seq, before_seq FROM backfills",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(Backfill {
                    conversation_id: uuid(r.get(0)?)?,
                    interval_seq: r.get(1)?,
                    until_seq: r.get(2)?,
                    before_seq: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn outbox(&self) -> anyhow::Result<Vec<OutboxItem>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT json FROM outbox ORDER BY created_at_ms")?;
        let rows = st
            .query_map([], |r| json(r.get(0)?))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn drafts(&self) -> anyhow::Result<Vec<DraftView>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT json FROM drafts")?;
        let rows = st
            .query_map([], |r| json(r.get(0)?))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn read_cursor(&self, conversation_id: Uuid) -> anyhow::Result<i64> {
        Ok(self
            .lock()
            .query_row(
                "SELECT seq FROM read_cursors WHERE conversation_id = ?1",
                [conversation_id.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    fn unread(&self, conversation_id: Uuid, me: Uuid) -> anyhow::Result<(u32, u32)> {
        let conn = self.lock();
        let conv = conversation_id.to_string();
        let me_s = me.to_string();
        let cursor: i64 = conn
            .query_row(
                "SELECT seq FROM read_cursors WHERE conversation_id = ?1",
                [&conv],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mention = format!("%\"user_id\":\"{me_s}\"%");
        Ok(conn.query_row(
            "SELECT count(*), coalesce(sum(json LIKE ?4), 0) FROM messages
              WHERE conversation_id = ?1 AND created_seq > ?2 AND author_id <> ?3
                AND deleted = 0 AND thread_root_id IS NULL",
            params![conv, cursor, me_s, mention],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    fn latest_seq(&self, conversation_id: Uuid) -> anyhow::Result<i64> {
        Ok(self.lock().query_row(
            "SELECT coalesce(max(created_seq), 0) FROM messages WHERE conversation_id = ?1",
            [conversation_id.to_string()],
            |r| r.get(0),
        )?)
    }
}

fn thread_root_id_of(root: &Option<String>) -> Option<Uuid> {
    root.as_ref().and_then(|r| r.parse().ok())
}
