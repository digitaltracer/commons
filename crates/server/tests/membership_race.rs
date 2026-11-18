//! Membership changes in a workspace are serialized by an advisory lock taken first, so
//! creating a private channel cannot race demoting an admin into lasting access.
mod common;

use common::*;
use reqwest::Method;

#[tokio::test(flavor = "multi_thread")]
async fn creating_a_private_channel_while_demoting_an_admin_leaves_no_access() {
    let srv = server::testing::spawn(|_| {}).await;
    let owner = register(&srv, "owner").await;
    let bob = register(&srv, "bob").await;
    let w = workspace(&srv, &owner, &[&bob]).await;
    let role = format!("/workspaces/{}/members/{}", w.id, bob.id);
    for i in 0..30 {
        ok(
            &srv,
            Method::PATCH,
            &owner,
            &role,
            Some(serde_json::json!({"role": "admin"})),
        )
        .await;
        let name = format!("secret-{i}");
        let (created, _) = tokio::join!(
            channel(&srv, &owner, w.id, &name, true),
            ok(
                &srv,
                Method::PATCH,
                &owner,
                &role,
                Some(serde_json::json!({"role": "member"}))
            ),
        );
        let open: i64 = server::sqlx::query_scalar(
            "SELECT count(*) FROM access_intervals
              WHERE conversation_id = $1 AND user_id = $2 AND left_seq IS NULL",
        )
        .bind(created.id)
        .bind(bob.id)
        .fetch_one(srv.pool())
        .await
        .unwrap();
        assert_eq!(
            open, 0,
            "round {i}: the ex-admin kept access to {}",
            created.id
        );
    }
}
