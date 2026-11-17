//! Step 2: a delayed backfill page never rolls back an edit, resurrects a deleted message,
//! or lands after access ended.
use std::sync::Arc;

use client_core::account::{Account, AccountConfig, AccountEvent};
use client_core::platform::RecordingNotifier;
use client_core::store::{Op, SqliteStore};
use client_core::transport::HttpTransport;
use proto::api::MessagePage;
use proto::events::{AccessVia, WorkspaceEvent, WorkspaceEventBody};
use proto::model::{Conversation, ConversationKind, Message, User, Workspace};
use uuid::Uuid;

struct Fixture {
    acc: Account,
    ws: Uuid,
    conv: Conversation,
    me: Uuid,
}

fn fixture() -> Fixture {
    let me = Uuid::now_v7();
    let ws = Uuid::now_v7();
    let acc = Account::new(
        AccountConfig {
            id: Uuid::now_v7(),
            server_url: "http://127.0.0.1:9".into(),
            user: User {
                id: me,
                email: "me@x".into(),
                display_name: "me".into(),
            },
            data_dir: std::env::temp_dir(),
        },
        Arc::new(HttpTransport::new("http://127.0.0.1:9")),
        Arc::new(SqliteStore::in_memory().unwrap()),
        Some("t".into()),
        Arc::new(RecordingNotifier::default()),
        Arc::new(|_: Uuid, _: AccountEvent| {}),
    );
    acc.commit(vec![Op::AddWorkspace {
        workspace: Workspace {
            id: ws,
            name: "w".into(),
        },
        since: 0,
    }])
    .unwrap();
    let conv = Conversation {
        id: Uuid::now_v7(),
        workspace_id: ws,
        kind: ConversationKind::Private,
        name: Some("secret".into()),
        dm_user_ids: vec![],
    };
    Fixture { acc, ws, conv, me }
}

fn message(
    f: &Fixture,
    created: i64,
    revision: i64,
    updated: i64,
    body: &str,
    deleted: bool,
) -> Message {
    Message {
        id: Uuid::from_u128(created as u128),
        workspace_id: f.ws,
        conversation_id: f.conv.id,
        author_id: Uuid::nil(),
        client_id: Uuid::from_u128(1000 + created as u128),
        thread_root_id: None,
        body: body.into(),
        entities: vec![],
        content_type: "text/markdown".into(),
        encryption: None,
        attachments: vec![],
        reactions: vec![],
        revision,
        deleted,
        created_seq: created,
        updated_seq: updated,
        created_at_ms: 0,
        edited_at_ms: None,
    }
}

impl Fixture {
    fn event(&self, seq: i64, body: WorkspaceEventBody) {
        self.acc
            .apply_workspace_page(self.ws, &[WorkspaceEvent { seq, body }], seq)
            .unwrap();
    }
    fn added(&self, seq: i64) {
        self.event(
            seq,
            WorkspaceEventBody::MemberAdded {
                conversation: self.conv.clone(),
                user_id: self.me,
                via: AccessVia::Membership,
                has_access: true,
            },
        );
    }
    fn removed(&self, seq: i64) {
        self.event(
            seq,
            WorkspaceEventBody::MemberRemoved {
                conversation_id: self.conv.id,
                user_id: self.me,
                via: AccessVia::Membership,
                has_access: false,
            },
        );
    }
    fn page(&self, interval: i64, messages: Vec<Message>) -> bool {
        self.acc
            .apply_backfill_page(
                self.conv.id,
                MessagePage {
                    messages,
                    interval_seq: interval,
                    next_before_seq: None,
                },
            )
            .unwrap()
    }
    fn cached(&self) -> Vec<Message> {
        self.acc.store.messages(self.conv.id).unwrap()
    }
}

#[test]
fn late_page_after_live_edit_does_not_roll_back() {
    let f = fixture();
    f.added(10);
    assert_eq!(
        f.acc.store.backfills().unwrap().len(),
        1,
        "joining records a pending backfill"
    );
    f.event(
        12,
        WorkspaceEventBody::MessageEdited {
            message: message(&f, 5, 1, 12, "new", false),
        },
    );
    // The page was read before the edit committed.
    assert!(f.page(10, vec![message(&f, 5, 0, 5, "old", false)]));
    let m = &f.cached()[0];
    assert_eq!((m.body.as_str(), m.revision), ("new", 1));
    assert!(
        f.acc.store.backfills().unwrap().is_empty(),
        "backfill completed"
    );
}

#[test]
fn late_page_after_delete_does_not_resurrect() {
    let f = fixture();
    f.added(10);
    f.event(
        13,
        WorkspaceEventBody::MessageDeleted {
            message: message(&f, 5, 2, 13, "", true),
        },
    );
    assert!(f.page(
        10,
        vec![message(&f, 5, 1, 11, "edited before delete", false)]
    ));
    let m = &f.cached()[0];
    assert!(m.deleted, "tombstone kept");
    assert_eq!(m.body, "");
}

#[test]
fn late_page_after_removal_is_discarded() {
    let f = fixture();
    f.added(10);
    f.removed(20);
    assert!(
        !f.page(10, vec![message(&f, 5, 0, 5, "secret", false)]),
        "page must be discarded"
    );
    assert!(f.cached().is_empty());
    assert!(
        f.acc.store.conversation(f.conv.id).unwrap().is_none(),
        "conversation purged"
    );
}

#[test]
fn page_for_an_earlier_interval_is_discarded_after_rejoin() {
    let f = fixture();
    f.added(10);
    f.removed(20);
    f.added(30);
    assert!(!f.page(
        10,
        vec![message(&f, 5, 0, 5, "from the old interval", false)]
    ));
    assert!(f.cached().is_empty());
    assert!(f.page(30, vec![message(&f, 5, 0, 5, "fresh", false)]));
    assert_eq!(f.cached()[0].body, "fresh");
}
