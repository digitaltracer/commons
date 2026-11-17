//! Member lists, adding people to private channels and group DMs, and who may remove whom.
mod common;

use common::*;
use reqwest::Method;
use uuid::Uuid;

fn ids(v: &serde_json::Value) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = v["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["user_id"].as_str().unwrap().parse().unwrap())
        .collect();
    out.sort();
    out
}

fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn private_channel_members_are_listed_only_to_readers() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let carol = register(&srv, "carol").await;
    let ws = workspace(&srv, &alice, &[&bob, &carol]).await;
    let secret = channel(&srv, &alice, ws.id, "secret", true).await;
    let path = format!("/conversations/{}/members", secret.id);
    ok(
        &srv,
        Method::POST,
        &alice,
        &path,
        Some(serde_json::json!({"user_id": bob.id})),
    )
    .await;

    let list = ok(&srv, Method::GET, &bob, &path, None).await;
    assert_eq!(ids(&list), sorted(vec![alice.id, bob.id]));
    assert_eq!(list["members"][0]["display_name"], "alice");
    let (s, _) = call(&srv, Method::GET, Some(&carol.token), &path, None).await;
    assert_eq!(s, 403, "a non-member cannot list a private channel");

    // A plain member cannot remove someone else; the owner can, and anyone can leave.
    let (s, _) = call(
        &srv,
        Method::DELETE,
        Some(&bob.token),
        &format!("{path}/{}", alice.id),
        None,
    )
    .await;
    assert_eq!(s, 403);
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("{path}/{}", bob.id),
        None,
    )
    .await;
    let (s, _) = call(&srv, Method::GET, Some(&bob.token), &path, None).await;
    assert_eq!(s, 403, "removed members can no longer list");
    ok(
        &srv,
        Method::POST,
        &alice,
        &format!("/conversations/{}/leave", secret.id),
        None,
    )
    .await;
    let list = ok(&srv, Method::GET, &alice, &path, None).await;
    assert!(
        ids(&list).is_empty(),
        "the owner still reads it by role, but is no member"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn people_added_to_a_group_dm_see_its_history_and_the_key_follows() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let carol = register(&srv, "carol").await;
    let dave = register(&srv, "dave").await;
    let ws = workspace(&srv, &alice, &[&bob, &carol, &dave]).await;
    let dms = format!("/workspaces/{}/dms", ws.id);

    let dm = ok(
        &srv,
        Method::POST,
        &alice,
        &dms,
        Some(serde_json::json!({"user_ids": [bob.id]})),
    )
    .await;
    let (s, _) = call(
        &srv,
        Method::POST,
        Some(&alice.token),
        &format!("/conversations/{}/members", dm["id"].as_str().unwrap()),
        Some(serde_json::json!({"user_id": carol.id})),
    )
    .await;
    assert_eq!(s, 403, "a 1:1 DM stays 1:1");

    let group = ok(
        &srv,
        Method::POST,
        &alice,
        &dms,
        Some(serde_json::json!({"user_ids": [bob.id, carol.id]})),
    )
    .await;
    let gid: Uuid = group["id"].as_str().unwrap().parse().unwrap();
    post(&srv, &alice, gid, "before dave").await;
    let path = format!("/conversations/{gid}/members");
    ok(
        &srv,
        Method::POST,
        &bob,
        &path,
        Some(serde_json::json!({"user_id": dave.id})),
    )
    .await;
    let list = ok(&srv, Method::GET, &dave, &path, None).await;
    assert_eq!(
        ids(&list),
        sorted(vec![alice.id, bob.id, carol.id, dave.id])
    );
    let replay = ok(
        &srv,
        Method::GET,
        &alice,
        &format!("/sync/workspaces/{}?since=0", ws.id),
        None,
    )
    .await;
    let added = replay["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["body"]["type"] == "member_added")
        .unwrap();
    let in_event: Vec<Uuid> =
        serde_json::from_value(added["body"]["conversation"]["dm_user_ids"].clone()).unwrap();
    assert_eq!(
        sorted(in_event),
        sorted(vec![alice.id, bob.id, carol.id, dave.id]),
        "the event carries the new member set, so clients retitle the DM"
    );
    let history = ok(
        &srv,
        Method::GET,
        &dave,
        &format!("/conversations/{gid}/messages"),
        None,
    )
    .await;
    assert_eq!(history["messages"][0]["body"], "before dave");

    let again = ok(
        &srv,
        Method::POST,
        &carol,
        &dms,
        Some(serde_json::json!({"user_ids": [alice.id, bob.id, dave.id]})),
    )
    .await;
    assert_eq!(
        again["id"], group["id"],
        "opening a DM with the new set finds it"
    );

    // Nobody removes someone else from a group DM, not even the workspace owner.
    let (s, _) = call(
        &srv,
        Method::DELETE,
        Some(&alice.token),
        &format!("{path}/{}", dave.id),
        None,
    )
    .await;
    assert_eq!(s, 403);
    ok(
        &srv,
        Method::POST,
        &dave,
        &format!("/conversations/{gid}/leave"),
        None,
    )
    .await;
    let back = ok(
        &srv,
        Method::POST,
        &alice,
        &dms,
        Some(serde_json::json!({"user_ids": [bob.id, carol.id]})),
    )
    .await;
    assert_eq!(
        back["id"], group["id"],
        "after dave leaves, the key is the remaining set"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_hits_carry_their_author() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let notes = channel(&srv, &alice, ws.id, "notes", false).await;
    post(&srv, &alice, notes.id, "the migration guide").await;
    let r = ok(
        &srv,
        Method::GET,
        &alice,
        &format!("/workspaces/{}/search?q=migration", ws.id),
        None,
    )
    .await;
    assert_eq!(r["hits"][0]["author_id"], alice.id.to_string());
    assert!(r["hits"][0]["thread_root_id"].is_null());
}
