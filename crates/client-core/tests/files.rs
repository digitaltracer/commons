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

/// B8: files wait in the composer, upload as they're staged, and go out with the text as one
/// message. A failed upload holds the send until it is retried or removed.
#[tokio::test(flavor = "multi_thread")]
async fn staged_files_go_out_together() {
    use client_core::account::{Account, AccountConfig, AccountEvent, MAX_FILES};
    use client_core::platform::RecordingNotifier;
    use client_core::transport::HttpTransport;
    use proto::view::{StagedFile, StagedState};
    use std::sync::{Arc, Mutex};

    let srv = server::testing::spawn(|c| {
        c.job_workers = 0;
        c.max_file_bytes = 100_000;
    })
    .await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let dir = tempfile::tempdir().unwrap();
    let seen: Arc<Mutex<Vec<Vec<StagedFile>>>> = Arc::default();
    let record = seen.clone();
    let a = Account::new(
        AccountConfig {
            id: uuid::Uuid::now_v7(),
            server_url: srv.base_url.clone(),
            user: alice.user.clone(),
            data_dir: dir.path().join("alice"),
        },
        Arc::new(HttpTransport::new(&srv.base_url)),
        memory(),
        Some(alice.token.clone()),
        Arc::new(RecordingNotifier::default()),
        Arc::new(move |_, ev| {
            if let AccountEvent::Staged { files, .. } = ev {
                record.lock().unwrap().push(files);
            }
        }),
    );
    let b = account_with_store(&srv, &bob, memory());
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    b.run_backfills().await.unwrap();
    let room = general(&a, ws.id);
    let key = (room, None);

    let log = dir.path().join("crash.log");
    std::fs::write(&log, b"boom").unwrap();
    let chart = dir.path().join("chart.png");
    std::fs::write(&chart, png()).unwrap();
    let big = dir.path().join("big.bin");
    std::fs::write(&big, vec![7u8; 200_000]).unwrap();
    a.stage_file(key, &log).unwrap();
    a.stage_file(key, &chart).unwrap();
    a.stage_bytes(key, "../notes.md", b"# pasted").unwrap();
    let too_big = a.stage_file(key, &big).unwrap();
    assert!(std::fs::read_dir(dir.path().join("alice/staged-uploads")).is_ok());

    let settled = |a: &Account| {
        a.staged(key)
            .iter()
            .all(|f| !matches!(f.state, StagedState::Uploading))
    };
    eventually("uploads settle", || settled(&a)).await;
    let files = a.staged(key);
    assert_eq!(
        files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        ["crash.log", "chart.png", "notes.md", "big.bin"]
    );
    assert!(files[..3].iter().all(StagedFile::ready));
    let StagedState::Failed { error } = &files[3].state else {
        panic!("{files:?}")
    };
    assert!(error.contains("100000"), "{error}");
    assert!(
        seen.lock().unwrap().iter().any(|l| l.len() == 4),
        "the UI hears about each change"
    );

    // The failed file holds the send, and stays failed when retried.
    let refused = a
        .send_with_staged(key, "logs attached".into(), None, vec![])
        .unwrap_err();
    assert!(refused.to_string().contains("retry"), "{refused}");
    a.retry_staged(key, too_big);
    assert!(matches!(a.staged(key)[3].state, StagedState::Uploading));
    eventually("the retry settles", || settled(&a)).await;
    assert!(matches!(a.staged(key)[3].state, StagedState::Failed { .. }));
    a.remove_staged(key, too_big);
    assert_eq!(a.staged(key).len(), 3);

    let client_id = a
        .send_with_staged(key, "logs attached".into(), None, vec![])
        .unwrap();
    assert!(a.staged(key).is_empty());
    assert_eq!(
        std::fs::read_dir(dir.path().join("alice/staged-uploads"))
            .unwrap()
            .count(),
        0,
        "the pasted copy is gone"
    );
    assert_eq!(
        seen.lock().unwrap().last().unwrap(),
        &Vec::<StagedFile>::new()
    );
    a.process_outbox().await.unwrap();
    b.sync_once().await.unwrap();
    let m = b.store.message_by_client_id(client_id).unwrap().unwrap();
    assert_eq!(m.body, "logs attached");
    assert_eq!(
        m.attachments
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["crash.log", "chart.png", "notes.md"]
    );

    // Files alone send with an empty body; a thread's composer is separate.
    let thread_key = (room, Some(m.id));
    a.stage_bytes(thread_key, "reply.txt", b"x").unwrap();
    assert!(a.staged(key).is_empty());
    eventually("the reply's file uploads", || {
        a.staged(thread_key).iter().all(StagedFile::ready)
    })
    .await;
    let reply = a
        .send_with_staged(thread_key, String::new(), Some(m.id), vec![])
        .unwrap();
    a.process_outbox().await.unwrap();
    b.sync_once().await.unwrap();
    let r = b.store.message_by_client_id(reply).unwrap().unwrap();
    assert_eq!((r.thread_root_id, r.attachments.len()), (Some(m.id), 1));
    assert!(
        a.send_with_staged(key, " ".into(), None, vec![]).is_err(),
        "nothing to send"
    );

    // No more files than the server takes on one message.
    for i in 0..MAX_FILES {
        a.stage_bytes(key, &format!("{i}.txt"), b"x").unwrap();
    }
    let full = a.stage_bytes(key, "one-more.txt", b"x").unwrap_err();
    assert!(full.to_string().contains("at most"), "{full}");
    eventually("ten small uploads", || settled(&a)).await;
    let id = a.send_with_staged(key, "ten".into(), None, vec![]).unwrap();
    a.process_outbox().await.unwrap();
    b.sync_once().await.unwrap();
    let m = b.store.message_by_client_id(id).unwrap().unwrap();
    assert_eq!(m.attachments.len(), MAX_FILES);
}
