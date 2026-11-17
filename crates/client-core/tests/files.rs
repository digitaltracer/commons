//! Step 4 through the core: attach a file, receive it as a reference, fetch its bytes with
//! the account's session, pick up the thumbnail, and lose access on removal.
mod common;

use client_core::views;
use common::*;
use proto::model::Conversation;

fn png() -> Vec<u8> {
    let img = image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([x as u8, y as u8, 200]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_fetch_and_lose_access() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let room: Conversation = post(
        &srv,
        Some(&alice.token),
        &format!("/workspaces/{}/channels", ws.id),
        serde_json::json!({"name": "room", "kind": "private"}),
    )
    .await;
    let _: serde_json::Value = post(
        &srv,
        Some(&alice.token),
        &format!("/conversations/{}/members", room.id),
        serde_json::json!({"user_id": bob.user.id}),
    )
    .await;

    let a = account_with_store(&srv, &alice, memory());
    let b = account_with_store(&srv, &bob, memory());
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    b.run_backfills().await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart.png");
    let bytes = png();
    std::fs::write(&path, &bytes).unwrap();
    let client_id = a.attach_file(room.id, None, &path).await.unwrap();
    a.process_outbox().await.unwrap();

    b.sync_once().await.unwrap();
    let m = b
        .store
        .message_by_client_id(client_id)
        .unwrap()
        .expect("bob has the message");
    assert_eq!(m.attachments.len(), 1);
    let file = m.attachments[0].clone();
    assert_eq!(
        (file.name.as_str(), file.mime.as_str()),
        ("chart.png", "image/png")
    );
    assert!(!file.has_thumbnail);
    let view = views::attachment(b.id, &file);
    assert_eq!(
        view.url,
        format!("commons-file://localhost/{}/{}", b.id, file.id)
    );
    assert_eq!(view.thumbnail_url, None);

    let (got, mime) = b.fetch_file(file.id, false).await.unwrap();
    assert_eq!((got, mime.as_str()), (bytes, "image/png"));

    // The thumbnail job announces itself on the stream; the cached message picks it up.
    run_jobs(&srv).await;
    b.sync_once().await.unwrap();
    let m = b.store.message_by_client_id(client_id).unwrap().unwrap();
    assert!(m.attachments[0].has_thumbnail);
    assert!(
        views::attachment(b.id, &m.attachments[0])
            .thumbnail_url
            .is_some()
    );
    let (thumb, mime) = b.fetch_file(file.id, true).await.unwrap();
    assert_eq!(mime, "image/png");
    assert!(image::load_from_memory(&thumb).is_ok());

    let (s, _) = request(
        &srv,
        reqwest::Method::DELETE,
        &alice.token,
        &format!("/conversations/{}/members/{}", room.id, bob.user.id),
        None,
    )
    .await;
    assert_eq!(s, 204);
    let denied = b.fetch_file(file.id, false).await.unwrap_err();
    assert!(denied.to_string().contains("404"), "{denied}");
    assert!(b.fetch_file(file.id, true).await.is_err());
    assert!(a.fetch_file(file.id, false).await.is_ok());
}
