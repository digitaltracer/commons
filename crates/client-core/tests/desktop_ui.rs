//! Core support for the desktop UI: member lists, presence, notification preferences,
//! picked mentions, recent emoji, queued sends while offline, and signing in again.
mod common;

use std::sync::Arc;

use client_core::Core;
use client_core::account::{Account, AccountConfig, AccountEvent, merge_mentions};
use client_core::platform::{MemorySecrets, RecordingNotifier, SecretStore};
use client_core::transport::{HttpTransport, Transport};
use common::*;
use proto::model::{Entity, Preferences};
use proto::view::{ConnectionState, SessionEnd, SidebarView};
use reqwest::Method;
use uuid::Uuid;

fn conversation(acc: &Account, id: Uuid) -> proto::view::ConversationView {
    acc.view()
        .unwrap()
        .workspaces
        .into_iter()
        .flat_map(|w| w.conversations)
        .find(|c| c.id == id)
        .expect("conversation in view")
}

fn offline_twin(acc: &Account, p: &Person) -> Account {
    Account::new(
        AccountConfig {
            id: acc.id,
            server_url: "http://127.0.0.1:9".into(),
            user: p.user.clone(),
            data_dir: std::env::temp_dir(),
        },
        Arc::new(HttpTransport::new("http://127.0.0.1:9")),
        acc.store.clone(),
        Some(p.token.clone()),
        Arc::new(RecordingNotifier::default()),
        Arc::new(|_: Uuid, _: AccountEvent| {}),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn members_presence_and_mute() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let a = account_with_store(&srv, &alice, memory());
    let b = account_with_store(&srv, &bob, memory());
    a.start();
    eventually("alice online", || a.connection() == ConnectionState::Online).await;
    b.start();
    eventually("alice sees bob online", || {
        a.online().contains(&bob.user.id)
    })
    .await;
    eventually("bob sees alice, who was online first", || {
        b.online().contains(&alice.user.id)
    })
    .await;

    let general = general(&a, ws.id);
    let mut ids: Vec<Uuid> = a
        .conversation_members(general)
        .await
        .unwrap()
        .iter()
        .map(|m| m.user_id)
        .collect();
    ids.sort();
    let mut want = vec![alice.user.id, bob.user.id];
    want.sort();
    assert_eq!(ids, want);

    b.leave(general).await.unwrap();
    eventually("bob left", || {
        a.store.members(ws.id).is_ok()
            && b.store
                .conversation(general)
                .unwrap()
                .is_none_or(|(_, x)| x.is_none())
    })
    .await;
    a.add_member(general, bob.user.id).await.unwrap();
    assert_eq!(a.conversation_members(general).await.unwrap().len(), 2);

    assert!(!conversation(&a, general).muted);
    a.set_muted(general, true).await.unwrap();
    assert!(conversation(&a, general).muted);
    let (_, prefs) = request(&srv, Method::GET, &alice.token, "/me/preferences", None).await;
    let prefs: Preferences = serde_json::from_value(prefs).unwrap();
    assert_eq!(
        prefs.muted,
        vec![general],
        "the notify module reads what the UI set"
    );
    a.set_preferences(Preferences {
        keywords: vec!["deploy".into()],
        ..prefs
    })
    .await
    .unwrap();
    assert_eq!(
        a.preferences().unwrap().keywords,
        vec!["deploy".to_string()]
    );

    b.stop();
    eventually("bob goes offline for alice", || {
        !a.online().contains(&bob.user.id)
    })
    .await;
    a.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_picked_mention_tells_same_named_members_apart() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let sam1 = register(&srv, "Sam").await;
    let sam2 = register(&srv, "Sam").await;
    let ws = workspace(&srv, &alice, &[&sam1, &sam2]).await;
    let a = account_with_store(&srv, &alice, memory());
    a.sync_once().await.unwrap();
    let general = general(&a, ws.id);
    let body = "hi @Sam".to_string();
    let picked = vec![Entity::Mention {
        user_id: sam2.user.id,
        start: 3,
        end: 7,
    }];
    a.send_message_with(general, body, None, vec![], picked)
        .unwrap();
    a.process_outbox().await.unwrap();
    let sent = a.store.messages(general).unwrap();
    let m = sent.iter().find(|m| m.body == "hi @Sam").expect("sent");
    assert_eq!(
        m.entities,
        vec![Entity::Mention {
            user_id: sam2.user.id,
            start: 3,
            end: 7
        }]
    );
}

#[test]
fn picked_mentions_must_point_at_an_at_sign() {
    let u = Uuid::from_u128(1);
    let v = Uuid::from_u128(2);
    let m = |user_id, start, end| Entity::Mention {
        user_id,
        start,
        end,
    };
    let got = merge_mentions(
        "@Ann and @Bo",
        vec![m(u, 0, 4), m(u, 1, 4), m(u, 0, 99)],
        vec![m(v, 0, 4), m(v, 9, 12)],
    );
    assert_eq!(got, vec![m(u, 0, 4), m(v, 9, 12)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn recent_emoji_and_queued_sends_while_offline() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let a = account_with_store(&srv, &alice, memory());
    a.sync_once().await.unwrap();
    let general = general(&a, ws.id);
    a.send_message(general, "hello".into(), None, vec![])
        .unwrap();
    a.process_outbox().await.unwrap();
    let m = a.store.messages(general).unwrap().remove(0);
    a.toggle_reaction(general, m.id, "🎉".into()).unwrap();
    a.toggle_reaction(general, m.id, "👍".into()).unwrap();
    a.process_outbox().await.unwrap();
    a.sync_once().await.unwrap();
    a.toggle_reaction(general, m.id, "🎉".into()).unwrap();
    assert_eq!(
        a.recent_emoji().unwrap(),
        vec!["👍", "🎉"],
        "removing doesn't reorder"
    );

    let off = offline_twin(&a, &alice);
    off.start();
    eventually("a reconnect is scheduled", || {
        off.connection() == ConnectionState::Offline
            && off.view().unwrap().reconnect_at_ms.is_some()
    })
    .await;
    // The pending React from above plus a send: only sends count as waiting messages.
    off.send_message(general, "later".into(), None, vec![])
        .unwrap();
    assert_eq!(conversation(&off, general).pending, 1);
    off.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_session_signs_in_again_and_keeps_its_cache() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let _ws = workspace(&srv, &alice, &[]).await;
    let url = srv.base_url.clone();
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let sidebars = Arc::new(std::sync::Mutex::new(Vec::<SidebarView>::new()));
    let seen = sidebars.clone();
    let open = |secrets: Arc<MemorySecrets>| {
        let url = url.clone();
        let seen = seen.clone();
        Core::open(
            dir.path(),
            secrets,
            Arc::new(RecordingNotifier::default()),
            Arc::new(move |_: &str| Arc::new(HttpTransport::new(&url)) as Arc<dyn Transport>),
            Arc::new(move |ev| {
                if let proto::view::AppEvent::Sidebar { sidebar } = ev {
                    seen.lock().unwrap().push(sidebar);
                }
            }),
        )
        .unwrap()
    };
    let core = open(secrets.clone());
    let id = core
        .add_account(
            &srv.base_url,
            &alice.user.email,
            "correct horse battery",
            None,
            None,
        )
        .await
        .unwrap();
    let acc = core.account(id).unwrap();
    eventually("online", || acc.connection() == ConnectionState::Online).await;
    acc.send_message(general(&acc, _ws.id), "cached".into(), None, vec![])
        .unwrap();
    eventually("sent", || acc.store.outbox().unwrap().is_empty()).await;
    let token = secrets.get(id).unwrap().unwrap();
    let (s, _) = request(&srv, Method::POST, &token, "/auth/logout", None).await;
    assert!((200..300).contains(&s));
    acc.stop();
    drop(core);

    // The app starts again with the revoked token.
    let core = open(secrets.clone());
    let acc = core.account(id).unwrap();
    eventually("signed out", || {
        acc.connection() == ConnectionState::SignedOut
    })
    .await;
    eventually("the UI hears about it", || {
        sidebars.lock().unwrap().iter().any(|s| {
            s.accounts.iter().any(|a| {
                a.id == id
                    && a.connection == ConnectionState::SignedOut
                    && a.session_end == Some(SessionEnd::Revoked)
            })
        })
    })
    .await;

    let wrong = core.sign_in_again(id, "wrong password").await.unwrap_err();
    assert!(
        wrong
            .to_string()
            .starts_with("That password didn't work for"),
        "{wrong}"
    );
    assert_eq!(acc.session_end(), Some(SessionEnd::Revoked), "still ended");
    core.sign_in_again(id, "correct horse battery")
        .await
        .unwrap();
    eventually("online again", || {
        acc.connection() == ConnectionState::Online
    })
    .await;
    assert_eq!(acc.session_end(), None);
    assert_ne!(
        secrets.get(id).unwrap().unwrap(),
        token,
        "the new session is saved"
    );
    let general = general(&acc, _ws.id);
    assert!(
        acc.store
            .messages(general)
            .unwrap()
            .iter()
            .any(|m| m.body == "cached"),
        "same account, same cache"
    );
    acc.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_without_a_saved_token_says_so_and_signs_in_again() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let reachable = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let open = || {
        let url = srv.base_url.clone();
        let up = reachable.clone();
        Core::open(
            dir.path(),
            secrets.clone(),
            Arc::new(RecordingNotifier::default()),
            Arc::new(move |_: &str| {
                let u = if up.load(std::sync::atomic::Ordering::SeqCst) {
                    url.clone()
                } else {
                    "http://127.0.0.1:9".to_string()
                };
                Arc::new(HttpTransport::new(&u)) as Arc<dyn Transport>
            }),
            Arc::new(|_| {}),
        )
        .unwrap()
    };
    let core = open();
    let id = core
        .add_account(
            &srv.base_url,
            &alice.user.email,
            "correct horse battery",
            None,
            None,
        )
        .await
        .unwrap();
    core.account(id).unwrap().stop();
    drop(core);

    // The saved account comes back, but its token is gone from this device.
    secrets.delete(id).unwrap();
    let core = open();
    let acc = core.account(id).unwrap();
    eventually("signed out", || {
        acc.connection() == ConnectionState::SignedOut
    })
    .await;
    assert_eq!(acc.session_end(), Some(SessionEnd::NoToken));
    let view = core.sidebar().unwrap();
    assert_eq!(view.accounts[0].session_end, Some(SessionEnd::NoToken));

    reachable.store(false, std::sync::atomic::Ordering::SeqCst);
    let down = core
        .sign_in_again(id, "correct horse battery")
        .await
        .unwrap_err();
    assert!(down.to_string().starts_with("Couldn't reach"), "{down}");

    reachable.store(true, std::sync::atomic::Ordering::SeqCst);
    core.sign_in_again(id, "correct horse battery")
        .await
        .unwrap();
    eventually("online", || acc.connection() == ConnectionState::Online).await;
    assert_eq!(acc.session_end(), None);
    assert!(
        secrets.get(id).unwrap().is_some(),
        "the new session is saved"
    );
    acc.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_the_session_while_online_signs_out_without_retrying() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let _ws = workspace(&srv, &alice, &[]).await;
    let a = account_with_store(&srv, &alice, memory());
    a.start();
    eventually("online", || a.connection() == ConnectionState::Online).await;
    let (s, _) = request(&srv, Method::POST, &alice.token, "/auth/logout", None).await;
    assert!((200..300).contains(&s));
    eventually("signed out", || {
        a.connection() == ConnectionState::SignedOut
    })
    .await;
    // Signed out is final: no reconnect attempt follows.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(a.connection(), ConnectionState::SignedOut);
    assert_eq!(srv.state.hub.connections(alice.user.id), 0);
    a.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_an_account_ends_its_server_session() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let reachable = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let up = reachable.clone();
    let url = srv.base_url.clone();
    let core = Core::open(
        dir.path(),
        secrets.clone(),
        Arc::new(RecordingNotifier::default()),
        Arc::new(move |_: &str| {
            let u = if up.load(std::sync::atomic::Ordering::SeqCst) {
                url.clone()
            } else {
                "http://127.0.0.1:9".to_string()
            };
            Arc::new(HttpTransport::new(&u)) as Arc<dyn Transport>
        }),
        Arc::new(|_| {}),
    )
    .unwrap();
    let add = || {
        core.add_account(
            &srv.base_url,
            &alice.user.email,
            "correct horse battery",
            None,
            None,
        )
    };

    let id = add().await.unwrap();
    let token = secrets.get(id).unwrap().unwrap();
    assert!(
        core.remove_account(id).await.unwrap(),
        "the server ended it"
    );
    let (s, _) = request(&srv, Method::GET, &token, "/me", None).await;
    assert_eq!(s, 401, "the removed account's token no longer works");
    assert!(secrets.get(id).unwrap().is_none());

    // Logout fails: the account is still removed here, and the caller hears about it.
    let id = add().await.unwrap();
    let token = secrets.get(id).unwrap().unwrap();
    reachable.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(!core.remove_account(id).await.unwrap());
    assert!(core.account(id).is_err());
    assert!(secrets.get(id).unwrap().is_none());
    let (s, _) = request(&srv, Method::GET, &token, "/me", None).await;
    assert_eq!(s, 200, "the session may still be active on the server");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invite_link_signs_up_or_signs_in_and_joins() {
    let srv =
        server::testing::spawn(|c| c.registration = server::config::Registration::Invite).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let link = |role: &str| {
        let token = alice.token.clone();
        let role = role.to_string();
        let srv = &srv;
        async move {
            let (s, v) = request(
                srv,
                Method::POST,
                &token,
                &format!("/workspaces/{}/invites", ws.id),
                Some(serde_json::json!({"role": role})),
            )
            .await;
            assert_eq!(s, 200, "{v}");
            v["link"].as_str().unwrap().to_string()
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let url = srv.base_url.clone();
    let core = Core::open(
        dir.path(),
        Arc::new(MemorySecrets::default()),
        Arc::new(RecordingNotifier::default()),
        Arc::new(move |_: &str| Arc::new(HttpTransport::new(&url)) as Arc<dyn Transport>),
        Arc::new(|_| {}),
    )
    .unwrap();
    let in_workspace = |id: Uuid| {
        let acc = core.account(id).unwrap();
        move || {
            acc.view()
                .map(|v| v.workspaces.iter().any(|w| w.id == ws.id))
                .unwrap_or(false)
        }
    };

    // Someone new: the link fills the server and the account joins as it registers.
    let pasted = link("member").await;
    let (server_url, token) = proto::api::split_invite(&pasted);
    assert_eq!(
        server_url.as_deref(),
        Some(srv.state.config.public_url.as_str())
    );
    let id = core
        .add_account(
            &srv.base_url,
            "newcomer@example.org",
            "correct horse battery",
            Some("Newcomer"),
            Some(&token),
        )
        .await
        .unwrap();
    eventually("the newcomer has the workspace", in_workspace(id)).await;

    // Someone with an account on the server (from another workspace's invite): signing in
    // with a link accepts it.
    let elsewhere = workspace(&srv, &alice, &[]).await;
    let (s, v) = request(
        &srv,
        Method::POST,
        &alice.token,
        &format!("/workspaces/{}/invites", elsewhere.id),
        Some(serde_json::json!({"role": "member"})),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let bob = register_with_invite(&srv, v["link"].as_str().unwrap()).await;
    let pasted = link("guest").await;
    let id = core
        .add_account(
            &srv.base_url,
            &bob,
            "correct horse battery",
            None,
            Some(&proto::api::split_invite(&pasted).1),
        )
        .await
        .unwrap();
    eventually("bob has the workspace", in_workspace(id)).await;

    // A refused invite leaves no account behind.
    let used = proto::api::split_invite(&pasted).1;
    let before = core.accounts().len();
    let e = core
        .add_account(
            &srv.base_url,
            "late@example.org",
            "correct horse battery",
            Some("Late"),
            Some(&used),
        )
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("invite_used"), "{e:#}");
    assert_eq!(core.accounts().len(), before);
}

/// Registers through an invite link over HTTP and returns the email.
async fn register_with_invite(srv: &server::testing::TestServer, link: &str) -> String {
    let email = format!("bob-{}@example.org", Uuid::now_v7().simple());
    let (s, v) = {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/auth/register", srv.base_url))
            .json(
                &serde_json::json!({"email": email, "password": "correct horse battery",
                "display_name": "bob", "invite": link}),
            )
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap())
    };
    assert_eq!(s, 200, "{v}");
    email
}

/// A keychain whose reads fail, as when access to it is refused.
#[derive(Default)]
struct UnreadableSecrets(MemorySecrets);

impl SecretStore for UnreadableSecrets {
    fn get(&self, _: Uuid) -> anyhow::Result<Option<String>> {
        anyhow::bail!("keychain access refused")
    }
    fn set(&self, id: Uuid, token: &str) -> anyhow::Result<()> {
        self.0.set(id, token)
    }
    fn delete(&self, id: Uuid) -> anyhow::Result<()> {
        self.0.delete(id)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keychain_read_error_reports_the_session_as_maybe_active() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let dir = tempfile::tempdir().unwrap();
    let url = srv.base_url.clone();
    let core = Core::open(
        dir.path(),
        Arc::new(UnreadableSecrets::default()),
        Arc::new(RecordingNotifier::default()),
        Arc::new(move |_: &str| Arc::new(HttpTransport::new(&url)) as Arc<dyn Transport>),
        Arc::new(|_| {}),
    )
    .unwrap();
    let id = core
        .add_account(
            &srv.base_url,
            &alice.user.email,
            "correct horse battery",
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!core.remove_account(id).await.unwrap(), "not revoked");
    assert!(core.account(id).is_err(), "still removed here");
}
