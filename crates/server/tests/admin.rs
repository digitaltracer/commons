//! `commonsd admin …` operator commands, and the workspace role rules owners and admins use.
mod common;

use std::time::Duration;

use common::*;
use reqwest::Method;
use server::testing::TestServer;

async fn admin(srv: &TestServer, args: &[&str]) -> anyhow::Result<String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    server::admin::run(srv.pool(), &srv.state.config, &args).await
}

async fn socket_for(srv: &TestServer, p: &Person, ws: uuid::Uuid) -> Socket {
    let s = open_socket(
        srv,
        &p.token,
        user_seq(srv, p.id).await,
        &[(ws, head(srv, ws).await)],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s
}

async fn me(srv: &TestServer, token: &str) -> u16 {
    call(srv, Method::GET, Some(token), "/me", None).await.0
}

async fn try_login(srv: &TestServer, email: &str, password: &str) -> u16 {
    call(
        srv,
        Method::POST,
        None,
        "/auth/login",
        Some(serde_json::json!({"email": email, "password": password})),
    )
    .await
    .0
}

async fn role(srv: &TestServer, ws: uuid::Uuid, user: uuid::Uuid) -> Option<String> {
    server::sqlx::query_scalar(
        "SELECT role FROM workspace_members
          WHERE workspace_id = $1 AND user_id = $2 AND left_seq IS NULL",
    )
    .bind(ws)
    .bind(user)
    .fetch_optional(srv.pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn list_users_shows_every_account_and_its_state() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    workspace(&srv, &alice, &[&bob]).await;
    admin(&srv, &["deactivate", &bob.email]).await.unwrap();
    let out = admin(&srv, &["list-users"]).await.unwrap();
    let line = |email: &str| {
        out.lines()
            .find(|l| l.starts_with(email))
            .unwrap()
            .to_string()
    };
    assert!(line(&alice.email).contains(" active "), "{out}");
    assert!(line(&alice.email).contains("password"), "{out}");
    assert!(line(&bob.email).contains("deactivated"), "{out}");
    assert!(out.ends_with("2 accounts"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reset_password_prints_a_one_time_link_and_signs_the_account_out() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let w = workspace(&srv, &alice, &[]).await;
    let mut socket = socket_for(&srv, &alice, w.id).await;

    let out = admin(&srv, &["reset-password", &alice.email.to_uppercase()])
        .await
        .unwrap();
    assert!(out.contains("Revoked 1 session(s)."), "{out}");
    assert!(socket.closed_within(Duration::from_secs(5)).await);
    assert_eq!(me(&srv, &alice.token).await, 401);

    let prefix = format!("{}/reset#", srv.state.config.public_url);
    let link = out.lines().find(|l| l.starts_with(&prefix)).unwrap();
    let token = link.strip_prefix(&prefix).unwrap();
    let confirm = |password: &'static str| {
        call(
            &srv,
            Method::POST,
            None,
            "/auth/reset/confirm",
            Some(serde_json::json!({"token": token, "password": password})),
        )
    };
    assert_eq!(confirm("a brand new password").await.0, 204);
    assert_eq!(
        confirm("another new password").await.0,
        400,
        "the link works once"
    );
    assert_eq!(
        try_login(&srv, &alice.email, "a brand new password").await,
        200
    );
    assert_eq!(
        try_login(&srv, &alice.email, "correct horse battery").await,
        401
    );

    // The link opens a page that sets the password.
    let page = reqwest::get(format!("{}/reset", srv.base_url))
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(
        page.text()
            .await
            .unwrap()
            .contains("/v1/auth/reset/confirm")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_sessions_signs_the_account_out_everywhere() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let w = workspace(&srv, &alice, &[]).await;
    let second = login(&srv, &alice).await;
    let mut socket = socket_for(&srv, &alice, w.id).await;
    let out = admin(&srv, &["revoke-sessions", &alice.email])
        .await
        .unwrap();
    assert_eq!(out, format!("Revoked 2 session(s) of {}.", alice.email));
    assert!(socket.closed_within(Duration::from_secs(5)).await);
    assert_eq!(me(&srv, &alice.token).await, 401);
    assert_eq!(me(&srv, &second).await, 401);
    assert_eq!(
        try_login(&srv, &alice.email, "correct horse battery").await,
        200,
        "the password still works"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deactivated_account_cant_sign_in_until_reactivated() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let w = workspace(&srv, &alice, &[]).await;
    let mut socket = socket_for(&srv, &alice, w.id).await;

    let out = admin(&srv, &["deactivate", &alice.email]).await.unwrap();
    assert!(out.starts_with("Deactivated"), "{out}");
    assert!(socket.closed_within(Duration::from_secs(5)).await);
    assert_eq!(me(&srv, &alice.token).await, 401);
    assert_eq!(
        try_login(&srv, &alice.email, "correct horse battery").await,
        401
    );
    // A session that slipped in some other way is still refused.
    server::sqlx::query("UPDATE sessions SET revoked_at = NULL")
        .execute(srv.pool())
        .await
        .unwrap();
    assert_eq!(me(&srv, &alice.token).await, 401);

    let out = admin(&srv, &["deactivate", &alice.email]).await.unwrap();
    assert!(out.contains("already deactivated"), "{out}");
    let out = admin(&srv, &["reactivate", &alice.email]).await.unwrap();
    assert!(out.starts_with("Reactivated"), "{out}");
    assert_eq!(
        try_login(&srv, &alice.email, "correct horse battery").await,
        200
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn make_owner_promotes_a_member_or_joins_a_stranger() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let carol = register(&srv, "carol").await;
    let w = workspace(&srv, &alice, &[&bob]).await;
    let secret = channel(&srv, &alice, w.id, "secret", true).await;

    let out = admin(&srv, &["make-owner", &bob.email, &w.id.to_string()])
        .await
        .unwrap();
    assert!(out.contains("was member"), "{out}");
    assert_eq!(role(&srv, w.id, bob.id).await.as_deref(), Some("owner"));
    let (s, _) = call(
        &srv,
        Method::GET,
        Some(&bob.token),
        &format!("/conversations/{}/messages", secret.id),
        None,
    )
    .await;
    assert_eq!(s, 200, "an owner reads private channels");

    let out = admin(&srv, &["make-owner", &carol.email, "W"])
        .await
        .unwrap();
    assert!(out.contains("was not a member"), "{out}");
    assert_eq!(role(&srv, w.id, carol.id).await.as_deref(), Some("owner"));

    let out = admin(&srv, &["make-owner", &carol.email, "W"])
        .await
        .unwrap();
    assert!(out.contains("already an owner"), "{out}");

    workspace(&srv, &alice, &[]).await;
    let err = admin(&srv, &["make-owner", &carol.email, "W"])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("several workspaces"), "{err}");
    let err = admin(&srv, &["make-owner", "nobody@example.org", "W"])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no account"), "{err}");
}

#[tokio::test]
async fn unknown_commands_print_usage() {
    let srv = server::testing::spawn(|_| {}).await;
    let err = admin(&srv, &["drop-everything"]).await.unwrap_err();
    assert!(err.to_string().starts_with("usage: commonsd admin"));
    let err = admin(&srv, &["deactivate"]).await.unwrap_err();
    assert!(err.to_string().starts_with("usage: commonsd admin"));
}

async fn set_role(
    srv: &TestServer,
    by: &Person,
    ws: uuid::Uuid,
    who: &Person,
    role: &str,
) -> (u16, serde_json::Value) {
    call(
        srv,
        Method::PATCH,
        Some(&by.token),
        &format!("/workspaces/{ws}/members/{}", who.id),
        Some(serde_json::json!({"role": role})),
    )
    .await
}

async fn remove(
    srv: &TestServer,
    by: &Person,
    ws: uuid::Uuid,
    who: &Person,
) -> (u16, serde_json::Value) {
    call(
        srv,
        Method::DELETE,
        Some(&by.token),
        &format!("/workspaces/{ws}/members/{}", who.id),
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn only_owners_grant_or_remove_owner_and_the_last_owner_stays() {
    let srv = server::testing::spawn(|_| {}).await;
    let owner = register(&srv, "owner").await;
    let admin_ = register(&srv, "admin").await;
    let member = register(&srv, "member").await;
    let w = workspace(&srv, &owner, &[&admin_, &member]).await;
    assert_eq!(set_role(&srv, &owner, w.id, &admin_, "admin").await.0, 204);

    // An admin can't grant owner, demote an owner, or remove one.
    assert_eq!(set_role(&srv, &admin_, w.id, &member, "owner").await.0, 403);
    assert_eq!(set_role(&srv, &admin_, w.id, &owner, "member").await.0, 403);
    assert_eq!(remove(&srv, &admin_, w.id, &owner).await.0, 403);
    // A member can't change roles at all.
    assert_eq!(set_role(&srv, &member, w.id, &member, "admin").await.0, 403);

    // The last owner can't step down or leave.
    let (s, v) = set_role(&srv, &owner, w.id, &owner, "admin").await;
    assert_eq!((s, v["error"].as_str()), (403, Some("last_owner")), "{v}");
    let (s, v) = remove(&srv, &owner, w.id, &owner).await;
    assert_eq!((s, v["error"].as_str()), (403, Some("last_owner")), "{v}");

    // With a second owner, either may go.
    assert_eq!(set_role(&srv, &owner, w.id, &admin_, "owner").await.0, 204);
    assert_eq!(set_role(&srv, &admin_, w.id, &owner, "member").await.0, 204);
    assert_eq!(role(&srv, w.id, owner.id).await.as_deref(), Some("member"));
    let (s, v) = remove(&srv, &admin_, w.id, &admin_).await;
    assert_eq!((s, v["error"].as_str()), (403, Some("last_owner")), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn admins_toggle_guests_and_see_join_dates_and_presence() {
    let srv = server::testing::spawn(|_| {}).await;
    let owner = register(&srv, "owner").await;
    let admin_ = register(&srv, "admin").await;
    let member = register(&srv, "member").await;
    let w = workspace(&srv, &owner, &[&admin_, &member]).await;
    assert_eq!(set_role(&srv, &owner, w.id, &admin_, "admin").await.0, 204);
    assert_eq!(set_role(&srv, &admin_, w.id, &member, "guest").await.0, 204);
    assert_eq!(role(&srv, w.id, member.id).await.as_deref(), Some("guest"));
    assert_eq!(
        set_role(&srv, &admin_, w.id, &member, "member").await.0,
        204
    );

    let path = format!("/workspaces/{}/admin/members", w.id);
    let (s, _) = call(&srv, Method::GET, Some(&member.token), &path, None).await;
    assert_eq!(s, 403, "members don't get the admin list");
    let _online = socket_for(&srv, &member, w.id).await;
    let v = ok(&srv, Method::GET, &admin_, &path, None).await;
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 3);
    let now = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
    for m in members {
        let joined = m["joined_at_ms"].as_i64().unwrap();
        assert!((now - 60_000..=now + 60_000).contains(&joined), "{m}");
        let is_member = m["user_id"] == member.id.to_string();
        assert_eq!(m["online"], is_member, "{m}");
    }
}

/// The binary reads the same environment as the server and exits non-zero on failure.
#[tokio::test(flavor = "multi_thread")]
async fn the_commonsd_binary_runs_admin_commands() {
    let srv = server::testing::spawn(|_| {}).await;
    let alice = register(&srv, "alice").await;
    let bin = srv.state.config.extract_program.clone().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(&bin)
            .arg("admin")
            .args(args)
            .env_clear()
            .env("COMMONS_DATABASE_URL", &srv.state.config.database_url)
            .env("COMMONS_PUBLIC_URL", "https://chat.example.org")
            .output()
            .unwrap()
    };
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let out = run(&["list-users"]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains(&alice.email));
    let out = run(&["reset-password", &alice.email]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains("https://chat.example.org/reset#"));
    assert!(
        text(&out.stderr).contains("reset-password"),
        "the command is logged: {out:?}"
    );
    let out = run(&["deactivate", "nobody@example.org"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("no account"));
    assert!(run(&["help"]).status.success());
}
