//! Step 4 check: a 1 GB upload streams through to the store and server memory stays flat.
//! One test per binary, so the RSS reading is not disturbed by other tests.
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;

const GB: u64 = 1 << 30;
const CHUNK: usize = 256 * 1024;

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap()
        / 1024.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_gigabyte_upload_keeps_memory_flat() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let alice = register(&srv, "alice").await;
    let ws = workspace(&srv, &alice, &[]).await;
    let c = channel(&srv, &alice, ws.id, "big", false).await;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/conversations/{}/files", srv.base_url, c.id);
    // Warm up the upload path so one-off allocations are in the baseline.
    let warm = client
        .post(&url)
        .query(&[("name", "warm.bin")])
        .bearer_auth(&alice.token)
        .body(vec![7u8; 8 << 20])
        .send()
        .await
        .unwrap();
    assert_eq!(warm.status().as_u16(), 200);

    let baseline = rss_mb();
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let done = done.clone();
        tokio::spawn(async move {
            let mut peak = 0f64;
            while !done.load(Ordering::Relaxed) {
                peak = peak.max(rss_mb());
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            peak
        })
    };
    let chunk = bytes::Bytes::from(vec![0x5au8; CHUNK]);
    let n = (GB as usize) / CHUNK;
    let body = futures::stream::iter((0..n).map(move |_| Ok::<_, std::io::Error>(chunk.clone())));
    let started = Instant::now();
    let resp = client
        .post(&url)
        .query(&[("name", "big.bin")])
        .bearer_auth(&alice.token)
        .header("Content-Length", GB)
        .body(reqwest::Body::wrap_stream(body))
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: serde_json::Value = resp.json().await.unwrap();
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);
    let peak = sampler.await.unwrap().max(rss_mb());
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["file"]["size"].as_u64(), Some(GB));

    let id = v["file"]["id"].as_str().unwrap();
    let path = std::path::Path::new(&srv.state.config.files_dir).join(format!("files/{id}"));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), GB);
    let _ = std::fs::remove_dir_all(&srv.state.config.files_dir);

    let growth = peak - baseline;
    println!(
        "1 GiB upload in {:.1} s; rss baseline {baseline:.0} MB, peak {peak:.0} MB, growth {growth:.0} MB",
        elapsed.as_secs_f64()
    );
    assert!(
        growth < 64.0,
        "server memory grew by {growth:.0} MB during a 1 GB upload"
    );
}
