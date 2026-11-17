//! Step 2 property test. Clients drop and reconnect while memberships are granted, revoked,
//! downgraded and rejoined, sends queue offline, and logs are pruned under them. Asserts:
//! - every payload a client receives was readable by that user at delivery;
//! - each client's cache ends equal to the server's state projected to that client;
//! - `next_since` always advances, so no client loops on a fully filtered range;
//! - a send queued offline is delivered at most once, and none is left pending.
mod common;

use client_core::account::Account;
use client_core::store::{OutboxState, SqliteStore};
use common::*;
use proptest::prelude::*;
use proto::events::WorkspaceEventBody;
use proto::model::Conversation;
use reqwest::Method;
use server::sqlx;
use server::testing::TestServer;
use uuid::Uuid;

const CLIENTS: usize = 3;
const CONVS: usize = 4;

#[derive(Debug, Clone)]
enum Act {
    Post { who: usize, conv: usize },
    Grant { who: usize, conv: usize },
    Revoke { who: usize, conv: usize },
    Promote { who: usize },
    Demote { who: usize },
    Remove { who: usize },
    Readd { who: usize },
    Offline { who: usize },
    Online { who: usize },
    Sync { who: usize },
    QueueSend { who: usize, conv: usize },
    Prune,
}

fn act() -> impl Strategy<Value = Act> {
    let who = 0..CLIENTS;
    let conv = 0..CONVS;
    prop_oneof![
        4 => (0..=CLIENTS, conv.clone()).prop_map(|(who, conv)| Act::Post { who, conv }),
        3 => (who.clone(), conv.clone()).prop_map(|(who, conv)| Act::Grant { who, conv }),
        2 => (who.clone(), conv.clone()).prop_map(|(who, conv)| Act::Revoke { who, conv }),
        1 => who.clone().prop_map(|who| Act::Promote { who }),
        1 => who.clone().prop_map(|who| Act::Demote { who }),
        1 => who.clone().prop_map(|who| Act::Remove { who }),
        1 => who.clone().prop_map(|who| Act::Readd { who }),
        2 => who.clone().prop_map(|who| Act::Offline { who }),
        2 => who.clone().prop_map(|who| Act::Online { who }),
        3 => who.clone().prop_map(|who| Act::Sync { who }),
        2 => (who, conv).prop_map(|(who, conv)| Act::QueueSend { who, conv }),
        1 => Just(Act::Prune),
    ]
}

struct World {
    srv: TestServer,
    alice: Person,
    people: Vec<Person>,
    clients: Vec<Account>,
    online: Vec<bool>,
    seen: Vec<usize>,
    ws: Uuid,
    convs: Vec<Conversation>,
    queued: Vec<(usize, Uuid)>,
}

impl World {
    fn person(&self, i: usize) -> &Person {
        if i == CLIENTS {
            &self.alice
        } else {
            &self.people[i]
        }
    }

    async fn admin(&self, method: Method, path: String, body: Option<serde_json::Value>) {
        let _ = request(&self.srv, method, &self.alice.token, &path, body).await;
    }

    async fn sync(&mut self, who: usize) {
        let acc = self.clients[who].clone();
        acc.sync_once()
            .await
            .unwrap_or_else(|e| panic!("client {who} sync: {e}"));
        acc.process_outbox().await.unwrap();
        acc.sync_once()
            .await
            .unwrap_or_else(|e| panic!("client {who} sync: {e}"));
        self.check_deliveries(who).await;
        self.check_next_since(who).await;
    }

    async fn readable_at(&self, user: Uuid, conv: Uuid, seq: i64) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM access_intervals WHERE conversation_id = $1 AND user_id = $2
                AND joined_seq <= $3 AND (left_seq IS NULL OR $3 < left_seq))
             AND EXISTS (SELECT 1 FROM access_intervals WHERE conversation_id = $1 AND user_id = $2
                AND left_seq IS NULL)",
        )
        .bind(conv)
        .bind(user)
        .bind(seq)
        .fetch_one(self.srv.pool())
        .await
        .unwrap()
    }

    async fn active(&self, user: Uuid) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM workspace_members WHERE workspace_id = $1
                AND user_id = $2 AND left_seq IS NULL)",
        )
        .bind(self.ws)
        .bind(user)
        .fetch_one(self.srv.pool())
        .await
        .unwrap()
    }

    /// Every event a client got must have been readable by its user when delivered.
    /// The driver is sequential, so "at delivery" is "now".
    async fn check_deliveries(&mut self, who: usize) {
        let me = self.people[who].user.id;
        let got: Vec<_> = self.clients[who].received.lock().unwrap()[self.seen[who]..].to_vec();
        self.seen[who] += got.len();
        for (_, ev) in got {
            let ok = match &ev.body {
                WorkspaceEventBody::MemberAdded { user_id, .. }
                | WorkspaceEventBody::MemberRemoved { user_id, .. }
                    if *user_id == me =>
                {
                    true
                }
                WorkspaceEventBody::ConversationCreated { .. }
                | WorkspaceEventBody::WorkspaceMemberJoined { .. }
                | WorkspaceEventBody::WorkspaceMemberLeft { .. }
                | WorkspaceEventBody::MemberRoleChanged { .. } => self.active(me).await,
                body => {
                    let conv = body.conversation_id().expect("conversation event");
                    self.readable_at(me, conv, ev.seq).await
                }
            };
            assert!(
                ok,
                "client {who} received an event it could not read: {ev:?}"
            );
        }
    }

    async fn check_next_since(&self, who: usize) {
        let me = self.people[who].user.id;
        if !self.active(me).await {
            return;
        }
        let (head, floor): (i64, i64) =
            sqlx::query_as("SELECT seq, log_floor FROM workspaces WHERE id = $1")
                .bind(self.ws)
                .fetch_one(self.srv.pool())
                .await
                .unwrap();
        for since in [floor, (floor + head) / 2, head.saturating_sub(1)] {
            if since < floor {
                continue;
            }
            let mut tx = self.srv.pool().begin().await.unwrap();
            let page = server::sync::read_workspace_page(&mut tx, self.ws, me, Some(since), 3)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert!(
                page.next_since > since || since == page.head,
                "next_since stuck at {since} (head {})",
                page.head
            );
        }
    }

    /// The cache equals the server's state projected to the client's user.
    async fn check_projection(&self, who: usize) {
        let me = self.people[who].user.id;
        let store = &self.clients[who].store;
        if !self.active(me).await {
            assert!(
                store.workspaces().unwrap().is_empty(),
                "client {who} kept a removed workspace"
            );
            return;
        }
        for c in &self.convs {
            let open: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM access_intervals WHERE conversation_id = $1
                    AND user_id = $2 AND left_seq IS NULL)",
            )
            .bind(c.id)
            .bind(me)
            .fetch_one(self.srv.pool())
            .await
            .unwrap();
            let cached_readable = matches!(store.conversation(c.id).unwrap(), Some((_, Some(_))));
            let cached: Vec<(Uuid, String, i64, bool)> = store
                .messages(c.id)
                .unwrap()
                .into_iter()
                .map(|m| (m.id, m.body, m.revision, m.deleted))
                .collect();
            if open {
                let server: Vec<(Uuid, String, i64, bool)> = sqlx::query_as(
                    "SELECT id, body, revision, deleted FROM messages WHERE conversation_id = $1
                      ORDER BY created_seq",
                )
                .bind(c.id)
                .fetch_all(self.srv.pool())
                .await
                .unwrap();
                assert!(cached_readable, "client {who} lost access to {:?}", c.name);
                assert_eq!(
                    cached, server,
                    "client {who} cache differs for {:?}",
                    c.name
                );
            } else {
                assert!(!cached_readable, "client {who} still reads {:?}", c.name);
                assert!(
                    cached.is_empty(),
                    "client {who} kept messages of {:?}",
                    c.name
                );
            }
        }
        assert!(
            store.backfills().unwrap().is_empty(),
            "client {who} left a backfill unfinished"
        );
    }

    async fn apply(&mut self, act: &Act) {
        let ws = self.ws;
        match *act {
            Act::Post { who, conv } => {
                let p = self.person(who);
                let _ = request(
                    &self.srv,
                    Method::POST,
                    &p.token,
                    &format!("/conversations/{}/messages", self.convs[conv].id),
                    Some(serde_json::json!({"client_id": Uuid::now_v7(), "body": format!("from {who}")})),
                )
                .await;
            }
            Act::Grant { who, conv } => {
                let user = self.people[who].user.id;
                let c = &self.convs[conv];
                if conv == 0 {
                    let _ = request(
                        &self.srv,
                        Method::POST,
                        &self.people[who].token,
                        &format!("/conversations/{}/join", c.id),
                        None,
                    )
                    .await;
                } else {
                    self.admin(
                        Method::POST,
                        format!("/conversations/{}/members", c.id),
                        Some(serde_json::json!({"user_id": user})),
                    )
                    .await;
                }
            }
            Act::Revoke { who, conv } => {
                let user = self.people[who].user.id;
                self.admin(
                    Method::DELETE,
                    format!("/conversations/{}/members/{user}", self.convs[conv].id),
                    None,
                )
                .await;
            }
            Act::Promote { who } | Act::Demote { who } => {
                let role = if matches!(act, Act::Promote { .. }) {
                    "admin"
                } else {
                    "member"
                };
                let user = self.people[who].user.id;
                self.admin(
                    Method::PATCH,
                    format!("/workspaces/{ws}/members/{user}"),
                    Some(serde_json::json!({"role": role})),
                )
                .await;
            }
            Act::Remove { who } => {
                let user = self.people[who].user.id;
                self.admin(
                    Method::DELETE,
                    format!("/workspaces/{ws}/members/{user}"),
                    None,
                )
                .await;
            }
            Act::Readd { who } => {
                let email = self.people[who].user.email.clone();
                self.admin(
                    Method::POST,
                    format!("/workspaces/{ws}/members"),
                    Some(serde_json::json!({"email": email, "role": "member"})),
                )
                .await;
            }
            Act::Offline { who } => self.online[who] = false,
            Act::Online { who } => {
                self.online[who] = true;
                self.sync(who).await;
            }
            Act::Sync { who } => {
                if self.online[who] {
                    self.sync(who).await;
                }
            }
            Act::QueueSend { who, conv } => {
                let id = self.clients[who]
                    .send_message(
                        self.convs[conv].id,
                        format!("queued by {who}"),
                        None,
                        vec![],
                    )
                    .unwrap();
                self.queued.push((who, id));
            }
            Act::Prune => {
                let mut tx = self.srv.pool().begin().await.unwrap();
                let head: i64 = sqlx::query_scalar("SELECT seq FROM workspaces WHERE id = $1")
                    .bind(ws)
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                server::sync::prune_workspace_to(&mut tx, ws, head)
                    .await
                    .unwrap();
                let users: Vec<(Uuid, i64)> = sqlx::query_as("SELECT id, seq FROM users")
                    .fetch_all(&mut *tx)
                    .await
                    .unwrap();
                for (u, seq) in users {
                    server::sync::prune_user_to(&mut tx, u, seq).await.unwrap();
                }
                tx.commit().await.unwrap();
            }
        }
    }
}

async fn run(acts: Vec<Act>) {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let mut people = vec![];
    for i in 0..CLIENTS {
        people.push(register(&srv, &format!("u{i}")).await);
    }
    let refs: Vec<&Person> = people.iter().collect();
    let w = workspace(&srv, &alice, &refs).await;
    let general = {
        let (_, v) = request(
            &srv,
            Method::GET,
            &alice.token,
            &format!("/workspaces/{}/conversations", w.id),
            None,
        )
        .await;
        serde_json::from_value::<Conversation>(v["conversations"][0].clone()).unwrap()
    };
    let mut convs = vec![general];
    for name in ["secret", "secret2"] {
        let c: Conversation = post(
            &srv,
            Some(&alice.token),
            &format!("/workspaces/{}/channels", w.id),
            serde_json::json!({"name": name, "kind": "private"}),
        )
        .await;
        convs.push(c);
    }
    let dm: Conversation = post(
        &srv,
        Some(&alice.token),
        &format!("/workspaces/{}/dms", w.id),
        serde_json::json!({"user_ids": [people[0].user.id]}),
    )
    .await;
    convs.push(dm);
    let clients = people
        .iter()
        .map(|p| {
            account_with_store(
                &srv,
                p,
                std::sync::Arc::new(SqliteStore::in_memory().unwrap()),
            )
        })
        .collect();
    let mut world = World {
        srv,
        alice,
        people,
        clients,
        online: vec![true; CLIENTS],
        seen: vec![0; CLIENTS],
        ws: w.id,
        convs,
        queued: vec![],
    };
    for who in 0..CLIENTS {
        world.sync(who).await;
    }
    for a in &acts {
        world.apply(a).await;
    }
    // One round delivers every outbox; the next lets everyone see the results.
    for _round in 0..2 {
        for who in 0..CLIENTS {
            world.online[who] = true;
            world.sync(who).await;
        }
    }
    for who in 0..CLIENTS {
        world.check_projection(who).await;
    }
    // Offline sends: delivered at most once, and none left pending.
    let dupes: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT client_id, count(*) FROM messages GROUP BY client_id HAVING count(*) > 1",
    )
    .fetch_all(world.srv.pool())
    .await
    .unwrap();
    assert!(dupes.is_empty(), "duplicate deliveries: {dupes:?}");
    for (who, id) in &world.queued {
        let on_server: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE client_id = $1)")
                .bind(id)
                .fetch_one(world.srv.pool())
                .await
                .unwrap();
        let outbox = world.clients[*who].store.outbox().unwrap();
        let item = outbox.iter().find(|o| o.id == *id);
        match item {
            None => assert!(
                on_server,
                "queued send {id} vanished without being delivered"
            ),
            Some(o) => {
                assert_eq!(
                    o.state,
                    OutboxState::Failed,
                    "queued send {id} still pending"
                );
                assert!(!on_server);
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(24),
        max_shrink_iters: 64,
        ..ProptestConfig::default()
    })]

    #[test]
    fn clients_converge_and_never_see_unreadable_events(acts in prop::collection::vec(act(), 10..60)) {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(run(acts));
    }
}
