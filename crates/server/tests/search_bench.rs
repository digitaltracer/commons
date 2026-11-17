//! Step 5 benchmark: search latency on a generated corpus, measured against the Tantivy
//! trigger in decision 12 (p95 above 500 ms). Results are recorded in ADR 0003.
//!
//! cargo test --release -p server --test search_bench -- --ignored --nocapture
mod common;

use std::time::{Duration, Instant};

use common::*;
use futures::StreamExt;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use server::sqlx;
use uuid::Uuid;

const MESSAGES: usize = 1_000_000;
const FILES_EVERY: usize = 50;
const CHANNELS: usize = 200;
const PRIVATE: usize = 50;
const VOCABULARY: usize = 50_000;
const RUNS_PER_KIND: usize = 60;
const TRIGGER_MS: f64 = 500.0;

const SYLLABLES: &[&str] = &[
    "ka", "lo", "mi", "ren", "to", "sa", "vel", "dor", "an", "is", "pe", "qu", "ri", "zo", "ul",
    "ne", "ba", "co", "fi", "gu", "ha", "jo", "ky", "ma", "no", "ox", "pi", "ra", "su", "te", "vi",
    "wa", "xe", "ya", "ze", "bri", "cla", "dro", "fle", "gri",
];

fn word(mut i: usize) -> String {
    let n = SYLLABLES.len();
    let mut w = String::new();
    loop {
        w.push_str(SYLLABLES[i % n]);
        i /= n;
        if i == 0 && w.len() >= 4 {
            return w;
        }
    }
}

/// Zipf-distributed word ranks (s = 1.07), the usual shape of natural-language text.
struct Zipf {
    cumulative: Vec<f64>,
}

impl Zipf {
    fn new(n: usize) -> Self {
        let mut acc = 0.0;
        let cumulative = (1..=n)
            .map(|r| {
                acc += 1.0 / (r as f64).powf(1.07);
                acc
            })
            .collect();
        Zipf { cumulative }
    }
    fn sample(&self, rng: &mut StdRng) -> usize {
        let total = *self.cumulative.last().unwrap();
        let x = rng.r#gen::<f64>() * total;
        self.cumulative.partition_point(|c| *c < x)
    }
}

fn sentence(rng: &mut StdRng, zipf: &Zipf, words: &[String], len: usize) -> String {
    (0..len)
        .map(|_| words[zipf.sample(rng)].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

type QueryGen<'a> = Box<dyn Fn(&mut StdRng) -> String + 'a>;

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run explicitly in release mode"]
async fn search_latency_on_the_benchmark_corpus() {
    let srv = server::testing::spawn(|c| c.job_workers = 0).await;
    let owner = register(&srv, "owner").await;
    let reader = register(&srv, "reader").await;
    let ws = workspace(&srv, &owner, &[&reader]).await.id;
    let pool = srv.pool();
    let words: Vec<String> = (0..VOCABULARY).map(word).collect();
    let zipf = Zipf::new(VOCABULARY);
    let mut rng = StdRng::seed_from_u64(7);
    let started = Instant::now();

    // Channels: the reader joins a third of the public ones and a fifth of the private ones,
    // so search covers every public channel plus ten private ones.
    let mut channels = vec![];
    for i in 0..CHANNELS {
        let id = Uuid::now_v7();
        let kind = if i < PRIVATE { "private" } else { "public" };
        sqlx::query(
            "INSERT INTO conversations (id, workspace_id, kind, name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(ws)
        .bind(kind)
        .bind(format!("chan-{i}"))
        .execute(pool)
        .await
        .unwrap();
        let joined = if i < PRIVATE { i % 5 == 0 } else { i % 3 == 0 };
        if joined {
            sqlx::query(
                "INSERT INTO access_intervals (conversation_id, user_id, via, joined_seq)
                 VALUES ($1, $2, 'membership', 1)",
            )
            .bind(id)
            .bind(reader.id)
            .execute(pool)
            .await
            .unwrap();
        }
        channels.push(id);
    }

    // Bulk-load without the GIN indexes, then build them once.
    for idx in ["messages_search", "files_text_search", "files_name_search"] {
        sqlx::query(&format!("DROP INDEX {idx}"))
            .execute(pool)
            .await
            .unwrap();
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let two_years = 2 * 365 * 24 * 3600;
    let mut conn = pool.acquire().await.unwrap();
    let mut copy = conn
        .copy_in_raw(
            "COPY messages (id, workspace_id, conversation_id, author_id, client_id, body,
                            created_seq, updated_seq, created_at) FROM STDIN",
        )
        .await
        .unwrap();
    let mut file_rows = String::new();
    let mut buf = String::new();
    for i in 0..MESSAGES {
        let id = Uuid::now_v7();
        let conv = channels[rng.gen_range(0..CHANNELS)];
        let len = rng.gen_range(4..40);
        let body = sentence(&mut rng, &zipf, &words, len);
        let at = now - two_years + (i as i64 * two_years / MESSAGES as i64);
        let t = time::OffsetDateTime::from_unix_timestamp(at).unwrap();
        let ts = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}+00",
            t.year(),
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        );
        buf.push_str(&format!(
            "{id}\t{ws}\t{conv}\t{}\t{}\t{body}\t{}\t{}\t{ts}\n",
            owner.id,
            Uuid::now_v7(),
            i + 1,
            i + 1
        ));
        if i % FILES_EVERY == 0 {
            let name = format!(
                "{}-{}.{}",
                words[zipf.sample(&mut rng)],
                words[zipf.sample(&mut rng)],
                ["pdf", "md", "txt", "rs"][i % 4]
            );
            let text = sentence(&mut rng, &zipf, &words, 300);
            file_rows.push_str(&format!(
                "{}\t{ws}\t{conv}\t{}\t{id}\t{name}\ttext/plain\t{}\tdone\t{text}\t{ts}\n",
                Uuid::now_v7(),
                owner.id,
                text.len()
            ));
        }
        if buf.len() > 4 << 20 {
            copy.send(buf.as_bytes()).await.unwrap();
            buf.clear();
        }
    }
    copy.send(buf.as_bytes()).await.unwrap();
    copy.finish().await.unwrap();
    let mut copy = conn
        .copy_in_raw(
            "COPY files (id, workspace_id, conversation_id, uploader_id, message_id, name, mime,
                         size, text_state, extracted_text, created_at) FROM STDIN",
        )
        .await
        .unwrap();
    copy.send(file_rows.as_bytes()).await.unwrap();
    copy.finish().await.unwrap();
    let loaded = started.elapsed();
    for sql in [
        "CREATE INDEX messages_search ON messages USING gin (search)",
        "CREATE INDEX files_name_search ON files USING gin (name_search)",
        "CREATE INDEX files_text_search ON files USING gin (text_search)",
        "ANALYZE",
    ] {
        sqlx::query(sql).execute(&mut *conn).await.unwrap();
    }
    drop(conn);
    let (msgs, files, size): (i64, i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM files),
                pg_size_pretty(pg_database_size(current_database()))",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    println!(
        "corpus: {msgs} messages, {files} files, {CHANNELS} channels, database {size}; \
         loaded in {:.0} s, indexed in {:.0} s",
        loaded.as_secs_f64(),
        (started.elapsed() - loaded).as_secs_f64()
    );

    // Query mix by word frequency. Common words match ~100k messages; rare ones a handful.
    let pick = |rng: &mut StdRng, lo: usize, hi: usize| words[rng.gen_range(lo..hi)].clone();
    let kinds: Vec<(&str, QueryGen)> = vec![
        (
            "rare word",
            Box::new(|r: &mut StdRng| pick(r, 20_000, VOCABULARY)),
        ),
        ("mid word", Box::new(|r: &mut StdRng| pick(r, 1_000, 5_000))),
        ("common word", Box::new(|r: &mut StdRng| pick(r, 5, 100))),
        (
            "two words",
            Box::new(|r: &mut StdRng| format!("{} {}", pick(r, 5, 500), pick(r, 500, 5_000))),
        ),
        (
            "phrase",
            Box::new(|r: &mut StdRng| format!("\"{} {}\"", pick(r, 0, 200), pick(r, 0, 200))),
        ),
    ];
    let client = reqwest::Client::new();
    let url = format!("{}/v1/workspaces/{ws}/search", srv.base_url);
    let run = |q: String| {
        let client = client.clone();
        let url = url.clone();
        let token = reader.token.clone();
        async move {
            let t = Instant::now();
            let resp = client
                .get(&url)
                .query(&[("q", q.as_str())])
                .bearer_auth(&token)
                .timeout(Duration::from_secs(60))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 200, "{q}");
            let r: proto::api::SearchResults = resp.json().await.unwrap();
            (t.elapsed().as_secs_f64() * 1000.0, r.hits.len())
        }
    };
    for _ in 0..20 {
        run(pick(&mut rng, 0, VOCABULARY)).await;
    }
    let mut all = vec![];
    for (name, gen_q) in &kinds {
        let mut times = vec![];
        let mut empty = 0;
        for _ in 0..RUNS_PER_KIND {
            let (ms, n) = run(gen_q(&mut rng)).await;
            times.push(ms);
            empty += usize::from(n == 0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{name:>12}: p50 {:>6.1} ms  p95 {:>6.1} ms  max {:>6.1} ms  ({empty}/{RUNS_PER_KIND} empty)",
            pct(&times, 0.50),
            pct(&times, 0.95),
            times.last().unwrap()
        );
        all.extend(times);
    }
    all.sort_by(f64::total_cmp);
    let p95 = pct(&all, 0.95);
    println!(
        "{:>12}: p50 {:>6.1} ms  p95 {:>6.1} ms  p99 {:>6.1} ms  ({} queries, one at a time)",
        "all",
        pct(&all, 0.50),
        p95,
        pct(&all, 0.99),
        all.len()
    );

    // Eight searchers at once.
    let mut queries = vec![];
    for i in 0..(8 * 40) {
        let (_, g) = &kinds[i % kinds.len()];
        queries.push(g(&mut rng));
    }
    let t = Instant::now();
    let mut conc: Vec<f64> = futures::stream::iter(queries.into_iter().map(&run))
        .buffer_unordered(8)
        .map(|(ms, _)| ms)
        .collect()
        .await;
    conc.sort_by(f64::total_cmp);
    println!(
        "{:>12}: p50 {:>6.1} ms  p95 {:>6.1} ms  ({} queries in {:.1} s, 8 at a time)",
        "concurrent",
        pct(&conc, 0.50),
        pct(&conc, 0.95),
        conc.len(),
        t.elapsed().as_secs_f64()
    );
    println!(
        "Tantivy trigger (p95 > {TRIGGER_MS} ms): {}",
        if p95 > TRIGGER_MS {
            "FIRED"
        } else {
            "not fired"
        }
    );
}
