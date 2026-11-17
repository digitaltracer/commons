//! Step 1 check: a reader never observes seq k+1 before k has committed, and no event is
//! published without its row.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use proto::events::WorkspaceEventBody;
use proto::model::Role;
use rand::Rng;
use server::sqlx;
use server::sqlx::postgres::PgListener;
use server::sync::{self, NewEvent};
use uuid::Uuid;

async fn setup() -> (sqlx::PgPool, Uuid, Uuid) {
    let url = server::testing::fresh_database().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(40)
        .connect(&url)
        .await
        .unwrap();
    let ws = Uuid::now_v7();
    let user = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name) VALUES ($1, 'r@x', '', 'r')",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id, role, display_name, joined_seq) VALUES ($1, $2, 'member', 'r', 0)")
        .bind(ws)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    (pool, ws, user)
}

fn event() -> NewEvent {
    NewEvent::workspace(WorkspaceEventBody::MemberRoleChanged {
        user_id: Uuid::nil(),
        role: Role::Member,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn reader_never_sees_k_plus_1_before_k() {
    let (pool, ws, user) = setup().await;
    let writers = 12;
    let per_writer = 40;
    let done = Arc::new(AtomicBool::new(false));

    let mut handles = vec![];
    for _ in 0..writers {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..per_writer {
                let (pre, post, n) = {
                    let mut rng = rand::thread_rng();
                    (
                        rng.gen_range(0..5u64),
                        rng.gen_range(0..15u64),
                        rng.gen_range(1..4usize),
                    )
                };
                let mut tx = pool.begin().await.unwrap();
                tokio::time::sleep(Duration::from_millis(pre)).await;
                sync::append(&mut tx, ws, (0..n).map(|_| event()).collect())
                    .await
                    .unwrap();
                // Forced commit delay: the seq is allocated, the commit has not happened.
                tokio::time::sleep(Duration::from_millis(post)).await;
                tx.commit().await.unwrap();
            }
        }));
    }

    // Raw readers resume again and again, from their own cursor or an earlier point.
    let mut readers = vec![];
    for r in 0..3 {
        let pool = pool.clone();
        let done = done.clone();
        readers.push(tokio::spawn(async move {
            let mut cursor = 0i64;
            let mut observations = 0u64;
            loop {
                let finished = done.load(Ordering::SeqCst);
                let seqs: Vec<i64> = sqlx::query_scalar(
                    "SELECT seq FROM events WHERE workspace_id = $1 AND seq > $2 ORDER BY seq",
                )
                .bind(ws)
                .bind(cursor)
                .fetch_all(&pool)
                .await
                .unwrap();
                for (i, s) in seqs.iter().enumerate() {
                    assert_eq!(
                        *s,
                        cursor + 1 + i as i64,
                        "reader {r} saw seq {s} after cursor {cursor} without its predecessor"
                    );
                }
                if let Some(last) = seqs.last() {
                    cursor = *last;
                }
                observations += 1;
                if finished {
                    return cursor;
                }
                if observations.is_multiple_of(7) {
                    cursor = (cursor - rand::thread_rng().gen_range(0..20)).max(0);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }));
    }

    // The filtered replay used by HTTPS and the socket must also never skip a seq.
    let replay = {
        let pool = pool.clone();
        let done = done.clone();
        tokio::spawn(async move {
            let mut since = 0i64;
            let mut seen = 0i64;
            loop {
                let finished = done.load(Ordering::SeqCst);
                let mut tx = pool.begin().await.unwrap();
                let page = sync::read_workspace_page(&mut tx, ws, user, Some(since), 50)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                for e in &page.events {
                    assert_eq!(e.seq, seen + 1, "replay skipped from {seen} to {}", e.seq);
                    seen = e.seq;
                }
                assert!(page.next_since >= since);
                since = page.next_since;
                if finished && since == page.head {
                    return seen;
                }
            }
        })
    };

    for h in handles {
        h.await.unwrap();
    }
    done.store(true, Ordering::SeqCst);
    let head: i64 = sqlx::query_scalar("SELECT seq FROM workspaces WHERE id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head, rows, "every allocated seq has exactly one row");
    for r in readers {
        assert_eq!(r.await.unwrap(), head);
    }
    assert_eq!(replay.await.unwrap(), head);
}

#[tokio::test]
async fn no_event_is_published_without_its_row() {
    let (pool, ws, _) = setup().await;
    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen("commons").await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    sync::append(&mut tx, ws, vec![event()]).await.unwrap();
    tx.rollback().await.unwrap();
    let got = tokio::time::timeout(Duration::from_millis(500), listener.recv()).await;
    assert!(got.is_err(), "a rolled-back append must not notify");
    let head: i64 = sqlx::query_scalar("SELECT seq FROM workspaces WHERE id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head, 0, "a rolled-back append must not advance the stream");

    let mut tx = pool.begin().await.unwrap();
    let seqs = sync::append(&mut tx, ws, vec![event(), event()])
        .await
        .unwrap();
    assert_eq!(seqs, vec![1, 2]);
    tx.commit().await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), listener.recv())
        .await
        .expect("notify after commit")
        .unwrap();
    assert_eq!(n.payload(), format!("w:{ws}"));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);
}
