//! Invite links: who may create them, what registration needs in invite mode, and joining
//! with one, as a new or an existing account.
mod common;

use common::*;
use proto::api::InviteCreated;
use reqwest::Method;
use server::config::Registration;
use server::testing::TestServer;

async fn spawn_invite_mode() -> TestServer {
    server::testing::spawn(|c| c.registration = Registration::Invite).await
}

async fn try_register(
    srv: &TestServer,
    email: &str,
    invite: Option<&str>,
) -> (u16, serde_json::Value) {
    call(
        srv,
        Method::POST,
        None,
        "/auth/register",
        Some(serde_json::json!({
            "email": email, "password": "correct horse battery", "display_name": "P",
            "invite": invite,
        })),
    )
    .await
}

async fn invite(srv: &TestServer, by: &Person, ws: uuid::Uuid, role: &str) -> InviteCreated {
    let v = ok(
        srv,
        Method::POST,
        by,
        &format!("/workspaces/{ws}/invites"),
        Some(serde_json::json!({"role": role})),
    )
    .await;
    serde_json::from_value(v).unwrap()
}

async fn role_of(srv: &TestServer, ws: uuid::Uuid, user: &str) -> Option<String> {
    server::sqlx::query_scalar(
        "SELECT wm.role FROM workspace_members wm JOIN users u ON u.id = wm.user_id
          WHERE wm.workspace_id = $1 AND u.email = $2 AND wm.left_seq IS NULL",
    )
    .bind(ws)
    .bind(user)
    .fetch_optional(srv.pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_account_needs_no_invite_and_the_next_one_does() {
    let srv = spawn_invite_mode().await;
    let (s, v) = try_register(&srv, "owner@example.org", None).await;
    assert_eq!(s, 200, "a fresh install can start: {v}");
    let (s, v) = try_register(&srv, "second@example.org", None).await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"], "invite_required");
    let (s, v) = try_register(&srv, "third@example.org", Some("not-a-token")).await;
    assert_eq!(s, 403);
    assert_eq!(v["error"], "invite_invalid");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_valid_invites_register() {
    let srv = spawn_invite_mode().await;
    let owner = register(&srv, "owner").await;
    let w = workspace(&srv, &owner, &[]).await;

    // Valid, given as the whole link: the account joins the workspace with the invite's role.
    let good = invite(&srv, &owner, w.id, "member").await;
    assert!(
        good.link.ends_with(&format!("/invite/{}", good.token)),
        "{}",
        good.link
    );
    let (s, v) = try_register(&srv, "new@example.org", Some(&good.link)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        role_of(&srv, w.id, "new@example.org").await.as_deref(),
        Some("member")
    );
    let (s, v) = replay(
        &srv,
        &Person {
            token: v["token"].as_str().unwrap().into(),
            id: owner.id,
            email: String::new(),
        },
        w.id,
        0,
    )
    .await;
    assert_eq!(s, 200, "the new account can read the workspace: {v}");

    // Used.
    let (s, v) = try_register(&srv, "again@example.org", Some(&good.token)).await;
    assert_eq!((s, v["error"].as_str()), (403, Some("invite_used")), "{v}");

    // Wrong secret for a real invite id.
    let other = invite(&srv, &owner, w.id, "member").await;
    let (id, _) = other.token.split_once('.').unwrap();
    let forged = format!("{id}.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let (s, v) = try_register(&srv, "forged@example.org", Some(&forged)).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (403, Some("invite_invalid")),
        "{v}"
    );

    // Revoked.
    ok(
        &srv,
        Method::DELETE,
        &owner,
        &format!("/workspaces/{}/invites/{}", w.id, other.invite.id),
        None,
    )
    .await;
    let (s, v) = try_register(&srv, "revoked@example.org", Some(&other.token)).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (403, Some("invite_revoked")),
        "{v}"
    );

    // Expired.
    let old = invite(&srv, &owner, w.id, "member").await;
    server::sqlx::query(
        "UPDATE invites SET expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(old.invite.id)
    .execute(srv.pool())
    .await
    .unwrap();
    let (s, v) = try_register(&srv, "late@example.org", Some(&old.token)).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (403, Some("invite_expired")),
        "{v}"
    );

    // None of the refusals left an account behind, and the listing shows each state.
    for email in ["again", "forged", "revoked", "late"] {
        let (s, _) = call(
            &srv,
            Method::POST,
            None,
            "/auth/login",
            Some(serde_json::json!({"email": format!("{email}@example.org"),
                "password": "correct horse battery"})),
        )
        .await;
        assert_eq!(s, 401, "{email}");
    }
    let list = ok(
        &srv,
        Method::GET,
        &owner,
        &format!("/workspaces/{}/invites", w.id),
        None,
    )
    .await;
    let list = list["invites"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert!(
        list.iter().all(|i| i.get("token").is_none()),
        "tokens are never listed"
    );
    assert!(list.iter().any(|i| !i["used_by"].is_null()));
    assert!(list.iter().any(|i| !i["revoked_at_ms"].is_null()));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_owners_and_admins_create_invites() {
    let srv = server::testing::spawn(|_| {}).await;
    let owner = register(&srv, "owner").await;
    let admin = register(&srv, "admin").await;
    let member = register(&srv, "member").await;
    let w = workspace(&srv, &owner, &[&admin, &member]).await;
    ok(
        &srv,
        Method::PATCH,
        &owner,
        &format!("/workspaces/{}/members/{}", w.id, admin.id),
        Some(serde_json::json!({"role": "admin"})),
    )
    .await;
    invite(&srv, &owner, w.id, "member").await;
    invite(&srv, &admin, w.id, "guest").await;
    let path = format!("/workspaces/{}/invites", w.id);
    let body = serde_json::json!({"role": "member"});
    let (s, _) = call(&srv, Method::POST, Some(&member.token), &path, Some(body)).await;
    assert_eq!(s, 403);
    let (s, _) = call(&srv, Method::GET, Some(&member.token), &path, None).await;
    assert_eq!(s, 403);
    let (s, _) = call(
        &srv,
        Method::POST,
        Some(&owner.token),
        &path,
        Some(serde_json::json!({"role": "admin"})),
    )
    .await;
    assert_eq!(s, 400, "invites grant member or guest only");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guest_invite_grants_the_guest_role() {
    let srv = spawn_invite_mode().await;
    let owner = register(&srv, "owner").await;
    let w = workspace(&srv, &owner, &[]).await;
    let inv = invite(&srv, &owner, w.id, "guest").await;
    assert_eq!(inv.invite.role, proto::model::Role::Guest);
    let (s, v) = try_register(&srv, "guest@example.org", Some(&inv.token)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        role_of(&srv, w.id, "guest@example.org").await.as_deref(),
        Some("guest")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_existing_user_accepts_an_invite() {
    let srv = spawn_invite_mode().await;
    let owner = register(&srv, "owner").await;
    let w = workspace(&srv, &owner, &[]).await;
    let inv = invite(&srv, &owner, w.id, "member").await;
    // Bob already has an account on this server, from another workspace's invite.
    let w2 = workspace(&srv, &owner, &[]).await;
    let first = invite(&srv, &owner, w2.id, "member").await;
    let (s, v) = try_register(&srv, "bob@example.org", Some(&first.token)).await;
    assert_eq!(s, 200, "{v}");
    let bob = Person {
        token: v["token"].as_str().unwrap().into(),
        id: v["user"]["id"].as_str().unwrap().parse().unwrap(),
        email: "bob@example.org".into(),
    };
    let since = user_seq(&srv, bob.id).await;

    let joined = ok(
        &srv,
        Method::POST,
        &bob,
        "/invites/accept",
        Some(serde_json::json!({"token": inv.link})),
    )
    .await;
    assert_eq!(joined["id"], w.id.to_string());
    assert_eq!(
        role_of(&srv, w.id, "bob@example.org").await.as_deref(),
        Some("member")
    );
    let page = ok(
        &srv,
        Method::GET,
        &bob,
        &format!("/sync/user?since={since}"),
        None,
    )
    .await;
    assert!(
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["body"]["type"] == "workspace_joined"
                && e["body"]["workspace"]["id"] == w.id.to_string()),
        "{page}"
    );

    // Already a member: refused, and the invite stays unused.
    let again = invite(&srv, &owner, w.id, "member").await;
    let (s, _) = call(
        &srv,
        Method::POST,
        Some(&bob.token),
        "/invites/accept",
        Some(serde_json::json!({"token": again.token})),
    )
    .await;
    assert_eq!(s, 409);
    let used: bool =
        server::sqlx::query_scalar("SELECT used_at IS NOT NULL FROM invites WHERE id = $1")
            .bind(again.invite.id)
            .fetch_one(srv.pool())
            .await
            .unwrap();
    assert!(!used);
    let (s, _) = call(
        &srv,
        Method::POST,
        None,
        "/invites/accept",
        Some(serde_json::json!({"token": again.token})),
    )
    .await;
    assert_eq!(s, 401, "accepting needs a signed-in user");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invite_link_opened_in_a_browser_explains_itself() {
    let srv = server::testing::spawn(|_| {}).await;
    let resp = reqwest::get(format!("{}/invite/abc.def", srv.base_url))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.text().await.unwrap().contains("Commons app"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_invite_costs_no_password_hash() {
    let srv = spawn_invite_mode().await;
    let owner = register(&srv, "owner").await;
    let w = workspace(&srv, &owner, &[]).await;
    let hashes = || {
        srv.state
            .password_hashes
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before = hashes();
    let real = invite(&srv, &owner, w.id, "member").await;
    let (id, _) = real.token.split_once('.').unwrap();
    for junk in [
        "junk".to_string(),
        format!("{}.x", uuid::Uuid::now_v7().simple()),
        format!("{id}.wrong-secret"),
    ] {
        let (s, v) = try_register(&srv, "j@example.org", Some(&junk)).await;
        assert_eq!(s, 403, "{v}");
    }
    let (s, _) = try_register(&srv, "nobody@example.org", None).await;
    assert_eq!(s, 403, "no invite, not the first account");
    assert_eq!(hashes(), before, "refused before hashing");
    let (s, v) = try_register(&srv, "real@example.org", Some(&real.token)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(hashes(), before + 1);
}
