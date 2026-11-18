//! Step 4 checks: downloads fail closed and are authorized on every request, uploads are
//! capped by quotas and finalized atomically, and the derived-content jobs survive a killed
//! worker without ever blocking the download.
mod common;

use std::time::Duration;

use common::*;
use proto::model::{FileInfo, Message};
use reqwest::Method;
use server::sqlx;
use server::testing::TestServer;
use uuid::Uuid;

async fn upload(
    srv: &TestServer,
    p: &Person,
    conv: Uuid,
    name: &str,
    bytes: Vec<u8>,
) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/conversations/{conv}/files", srv.base_url))
        .query(&[("name", name)])
        .bearer_auth(&p.token)
        .body(bytes)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

async fn upload_ok(srv: &TestServer, p: &Person, conv: Uuid, name: &str, bytes: &[u8]) -> FileInfo {
    let (s, v) = upload(srv, p, conv, name, bytes.to_vec()).await;
    assert_eq!(s, 200, "{v}");
    serde_json::from_value(v["file"].clone()).unwrap()
}

async fn fetch(srv: &TestServer, p: &Person, path: &str) -> (u16, Vec<u8>) {
    let resp = reqwest::Client::new()
        .get(format!("{}/v1{path}", srv.base_url))
        .bearer_auth(&p.token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.bytes().await.unwrap().to_vec())
}

async fn send_with(
    srv: &TestServer,
    p: &Person,
    conv: Uuid,
    files: &[Uuid],
) -> (u16, serde_json::Value) {
    call(
        srv,
        Method::POST,
        Some(&p.token),
        &format!("/conversations/{conv}/messages"),
        Some(serde_json::json!({"client_id": Uuid::now_v7(), "body": "", "attachments": files})),
    )
    .await
}

async fn run_jobs(srv: &TestServer) {
    while let Some(job) = server::jobs::claim(srv.pool(), Duration::from_secs(30))
        .await
        .unwrap()
    {
        server::jobs::run_one(&srv.state, job).await;
    }
}

async fn text_state(srv: &TestServer, id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT text_state, extracted_text FROM files WHERE id = $1")
        .bind(id)
        .fetch_one(srv.pool())
        .await
        .unwrap()
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_member_is_denied_on_the_next_request() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let secret = channel(&srv, &alice, ws.id, "secret", true).await;
    ok(
        &srv,
        Method::POST,
        &alice,
        &format!("/conversations/{}/members", secret.id),
        Some(serde_json::json!({"user_id": bob.id})),
    )
    .await;
    let file = upload_ok(&srv, &alice, secret.id, "plan.txt", b"the plan").await;
    let (s, v) = send_with(&srv, &alice, secret.id, &[file.id]).await;
    assert_eq!(s, 200, "{v}");
    let m: Message = serde_json::from_value(v).unwrap();
    assert_eq!(m.attachments, vec![file.clone()]);

    let path = format!("/files/{}", file.id);
    assert_eq!(fetch(&srv, &bob, &path).await, (200, b"the plan".to_vec()));
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/conversations/{}/members/{}", secret.id, bob.id),
        None,
    )
    .await;
    assert_eq!(
        fetch(&srv, &bob, &path).await.0,
        404,
        "removed member still reads the file"
    );
    assert_eq!(fetch(&srv, &alice, &path).await.0, 200);

    // Removal from the workspace cuts access the same way.
    let general = ok(
        &srv,
        Method::GET,
        &alice,
        &format!("/workspaces/{}/conversations", ws.id),
        None,
    )
    .await["conversations"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let shared = upload_ok(&srv, &alice, general, "all.txt", b"everyone").await;
    assert_eq!(send_with(&srv, &alice, general, &[shared.id]).await.0, 200);
    let path = format!("/files/{}", shared.id);
    assert_eq!(fetch(&srv, &bob, &path).await.0, 200);
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/workspaces/{}/members/{}", ws.id, bob.id),
        None,
    )
    .await;
    assert_eq!(fetch(&srv, &bob, &path).await.0, 404);
    // No session at all is refused before any lookup.
    let anon = reqwest::get(format!("{}/v1{path}", srv.base_url))
        .await
        .unwrap();
    assert_eq!(anon.status().as_u16(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_follow_the_message() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let one = channel(&srv, &alice, ws.id, "one", false).await;
    let two = channel(&srv, &alice, ws.id, "two", false).await;
    ok(
        &srv,
        Method::POST,
        &bob,
        &format!("/conversations/{}/join", one.id),
        None,
    )
    .await;

    let file = upload_ok(&srv, &alice, one.id, "../../etc/notes.md", b"# notes").await;
    assert_eq!(file.name, "notes.md", "paths are stripped from names");
    assert_eq!(file.mime, "text/markdown");
    let path = format!("/files/{}", file.id);
    // Unsent: only the uploader can read it.
    assert_eq!(fetch(&srv, &alice, &path).await.0, 200);
    assert_eq!(fetch(&srv, &bob, &path).await.0, 404);
    // Someone else's upload, or one from another conversation, cannot be attached.
    assert_eq!(send_with(&srv, &bob, one.id, &[file.id]).await.0, 400);
    assert_eq!(send_with(&srv, &alice, two.id, &[file.id]).await.0, 400);
    assert_eq!(
        send_with(&srv, &alice, one.id, &[Uuid::now_v7()]).await.0,
        400
    );

    let (s, v) = send_with(&srv, &alice, one.id, &[file.id]).await;
    assert_eq!(s, 200, "{v}");
    let m: Message = serde_json::from_value(v).unwrap();
    assert_eq!(fetch(&srv, &bob, &path).await.0, 200);
    // A file goes out on one message only.
    assert_eq!(send_with(&srv, &alice, one.id, &[file.id]).await.0, 400);
    // History carries the attachment.
    let page = ok(
        &srv,
        Method::GET,
        &bob,
        &format!("/conversations/{}/messages", one.id),
        None,
    )
    .await;
    assert_eq!(
        page["messages"][0]["attachments"][0]["id"],
        file.id.to_string()
    );

    // Deleting the message takes the file with it.
    ok(
        &srv,
        Method::DELETE,
        &alice,
        &format!("/messages/{}", m.id),
        None,
    )
    .await;
    assert_eq!(fetch(&srv, &bob, &path).await.0, 404);
    assert_eq!(fetch(&srv, &alice, &path).await.0, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_are_capped_and_never_left_half_written() {
    let srv = server::testing::spawn(|c| {
        c.job_workers = 0;
        c.max_file_bytes = 600;
        c.user_quota_bytes = 1000;
    })
    .await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let c = channel(&srv, &alice, ws.id, "c", false).await;

    assert_eq!(
        upload(&srv, &alice, c.id, "big.bin", vec![1; 700]).await.0,
        413
    );
    upload_ok(&srv, &alice, c.id, "a.bin", &[1; 500]).await;
    let (s, v) = upload(&srv, &alice, c.id, "b.bin", vec![1; 600]).await;
    assert_eq!(s, 413, "quota: {v}");
    upload_ok(&srv, &alice, c.id, "c.bin", &[1; 400]).await;

    // A body with no declared length is cut off once it passes the cap.
    let chunks = futures::stream::iter(
        (0..10).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![2u8; 100]))),
    );
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/conversations/{}/files", srv.base_url, c.id))
        .query(&[("name", "stream.bin")])
        .bearer_auth(&alice.token)
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 413);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert_eq!(rows, 2);
    // Only the two finished uploads exist in the store.
    let objects = count_files(std::path::Path::new(&srv.state.config.files_dir));
    assert_eq!(objects, 2, "refused uploads left objects behind");
}

fn count_files(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() { count_files(&p) } else { 1 }
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn thumbnails_and_text_are_derived_by_jobs() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let bob = register(&srv, "bob").await;
    let ws = workspace(&srv, &alice, &[&bob]).await;
    let c = channel(&srv, &alice, ws.id, "c", false).await;
    ok(
        &srv,
        Method::POST,
        &bob,
        &format!("/conversations/{}/join", c.id),
        None,
    )
    .await;

    let image = upload_ok(&srv, &alice, c.id, "photo.png", &png(1200, 800)).await;
    let notes = upload_ok(
        &srv,
        &alice,
        c.id,
        "notes.md",
        "Quarterly **zephyr** plan".as_bytes(),
    )
    .await;
    let blob = upload_ok(&srv, &alice, c.id, "data.bin", &[0u8, 1, 2, 3]).await;
    let since = head(&srv, ws.id).await;
    assert_eq!(
        send_with(&srv, &alice, c.id, &[image.id, notes.id, blob.id])
            .await
            .0,
        200
    );
    let thumb = format!("/files/{}/thumbnail", image.id);
    assert_eq!(
        fetch(&srv, &bob, &thumb).await.0,
        404,
        "no thumbnail before the job"
    );

    run_jobs(&srv).await;

    let (s, bytes) = fetch(&srv, &bob, &thumb).await;
    assert_eq!(s, 200);
    let t = image::load_from_memory(&bytes).unwrap();
    assert_eq!((t.width(), t.height()), (320, 213));
    // The message's readers hear about the thumbnail on the stream.
    let (_, replay) = replay(&srv, &bob, ws.id, since).await;
    let updated: Vec<_> = replay["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["body"]["type"] == "file_updated")
        .collect();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["body"]["file"]["has_thumbnail"], true);

    let (state, text) = text_state(&srv, notes.id).await;
    assert_eq!(state, "done");
    assert_eq!(text.as_deref(), Some("Quarterly **zephyr** plan"));
    assert_eq!(text_state(&srv, blob.id).await, ("none".to_string(), None));
    assert_eq!(text_state(&srv, image.id).await.0, "none");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_extraction_never_blocks_the_download() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let c = channel(&srv, &alice, ws.id, "c", false).await;
    let junk = b"%PDF-1.7 this is not really a pdf".to_vec();
    let file = upload_ok(&srv, &alice, c.id, "report.pdf", &junk).await;
    assert_eq!(send_with(&srv, &alice, c.id, &[file.id]).await.0, 200);
    // One attempt, so the job gives up now rather than after backoff.
    sqlx::query("UPDATE jobs SET max_attempts = 1")
        .execute(srv.pool())
        .await
        .unwrap();
    run_jobs(&srv).await;

    assert_eq!(
        text_state(&srv, file.id).await,
        ("failed".to_string(), None)
    );
    let failed: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE failed_at IS NOT NULL")
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert_eq!(failed, 1);
    assert_eq!(
        fetch(&srv, &alice, &format!("/files/{}", file.id)).await,
        (200, junk)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_killed_mid_job_is_retried() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let c = channel(&srv, &alice, ws.id, "c", false).await;
    let file = upload_ok(&srv, &alice, c.id, "main.rs", b"fn quixotic() {}").await;

    // A worker claims the job and dies before finishing: its lease is all that remains.
    let lease = Duration::from_millis(500);
    let first = server::jobs::claim(srv.pool(), lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.kind, server::files::EXTRACT_JOB);
    let killed = tokio::spawn(async move {
        let _claimed = first;
        std::future::pending::<()>().await;
    });
    killed.abort();
    assert!(
        server::jobs::claim(srv.pool(), lease)
            .await
            .unwrap()
            .is_none(),
        "a live lease must not be taken"
    );
    tokio::time::sleep(lease + Duration::from_millis(200)).await;

    let retry = server::jobs::claim(srv.pool(), lease)
        .await
        .unwrap()
        .expect("job reclaimed");
    assert_eq!(retry.kind, server::files::EXTRACT_JOB);
    assert_eq!(retry.attempts, 2);
    let id = retry.id;
    server::jobs::run_one(&srv.state, retry).await;

    let (state, text) = text_state(&srv, file.id).await;
    assert_eq!(
        (state.as_str(), text.as_deref()),
        ("done", Some("fn quixotic() {}"))
    );
    let done: bool = sqlx::query_scalar("SELECT done_at IS NOT NULL FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(srv.pool())
        .await
        .unwrap();
    assert!(done);
}

/// A one-page PDF whose text is `words`, with a correct xref table.
fn tiny_pdf(words: &str) -> Vec<u8> {
    let stream = format!("BT /F1 12 Tf 72 720 Td ({words}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![];
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .bytes(),
    );
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn pdf_text_is_extracted_in_a_child_process() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let c = channel(&srv, &alice, ws.id, "c", false).await;
    let file = upload_ok(
        &srv,
        &alice,
        c.id,
        "memo.pdf",
        &tiny_pdf("quarterly zeppelin"),
    )
    .await;
    assert_eq!(send_with(&srv, &alice, c.id, &[file.id]).await.0, 200);
    run_jobs(&srv).await;
    let (state, text) = text_state(&srv, file.id).await;
    assert_eq!(state, "done");
    assert!(text.unwrap().contains("zeppelin"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_extraction_is_killed_and_not_retried() {
    let dir = std::env::temp_dir().join(format!("commons-extract-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("pid");
    let script = dir.join("hang.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n",
            pidfile.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let program = script.clone();
    let srv = server::testing::spawn(move |c| {
        c.job_workers = 0;
        c.extract_program = Some(program);
        c.extract_timeout = Duration::from_secs(1);
    })
    .await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let c = channel(&srv, &alice, ws.id, "c", false).await;
    let pdf = tiny_pdf("never read");
    let file = upload_ok(&srv, &alice, c.id, "slow.pdf", &pdf).await;
    assert_eq!(send_with(&srv, &alice, c.id, &[file.id]).await.0, 200);
    run_jobs(&srv).await;

    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the extractor was killed and reaped");
    assert_eq!(
        text_state(&srv, file.id).await,
        ("failed".to_string(), None)
    );
    let (done, attempts): (bool, i32) = sqlx::query_as(
        "SELECT done_at IS NOT NULL, attempts FROM jobs WHERE kind = 'file_extract'",
    )
    .fetch_one(srv.pool())
    .await
    .unwrap();
    assert!(done && attempts == 1, "no retry after a timeout");
    assert_eq!(
        fetch(&srv, &alice, &format!("/files/{}", file.id)).await,
        (200, pdf)
    );
    let _ = std::fs::remove_dir_all(dir);
}
