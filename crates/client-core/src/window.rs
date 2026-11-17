//! Core side of windowed subscriptions (decision 4).
//!
//! Each subscription holds a bounded window. Store changes mark it dirty; a flush computes
//! the new rows and sends one batched delta. At most [`MAX_UNACKED`] batches are in flight;
//! past that, or when a delta would exceed [`MAX_BATCH_BYTES`], the window is reset instead
//! of queueing more. The UI resets a window on any gap.
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use proto::view::MessageView;
use proto::window::{MAX_WINDOW_ROWS, WindowBatch, WindowRow, WindowSpec};
use uuid::Uuid;

pub const MAX_UNACKED: u64 = 4;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Delivers a batch to the UI. Returns false when the receiver is gone.
pub type Sink = Box<dyn Fn(WindowBatch) -> bool + Send + Sync>;

/// Produces the current rows of a window: (total rows, rows oldest first).
pub trait RowSource: Send + Sync {
    fn rows(&self, spec: &WindowSpec) -> anyhow::Result<(u32, Vec<MessageView>)>;
}

struct Sub {
    spec: WindowSpec,
    sink: Sink,
    generation: u64,
    revision: u64,
    acked: u64,
    rows: Vec<(Uuid, u32)>,
    dirty: bool,
    /// Deltas were withheld because too many were unacknowledged; reset on the next ack.
    overflowed: bool,
}

pub struct WindowServer {
    subs: Mutex<HashMap<u64, Sub>>,
    next_id: AtomicU64,
    /// Counters for tests and the spike.
    pub resets: AtomicU64,
    pub deltas: AtomicU64,
}

impl Default for WindowServer {
    fn default() -> Self {
        WindowServer {
            subs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            resets: AtomicU64::new(0),
            deltas: AtomicU64::new(0),
        }
    }
}

fn version(view: &MessageView) -> u32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(view).unwrap_or_default().hash(&mut h);
    h.finish() as u32
}

fn to_rows(views: Vec<MessageView>) -> Vec<WindowRow> {
    views
        .into_iter()
        .map(|m| WindowRow {
            key: m.id,
            version: version(&m),
            message: m,
        })
        .collect()
}

fn clamp(mut spec: WindowSpec) -> WindowSpec {
    spec.limit = spec.limit.clamp(1, MAX_WINDOW_ROWS);
    spec
}

impl WindowServer {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Sub>> {
        self.subs.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn subscribe(&self, spec: WindowSpec, sink: Sink, src: &dyn RowSource) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut sub = Sub {
            spec: clamp(spec),
            sink,
            generation: 0,
            revision: 0,
            acked: 0,
            rows: vec![],
            dirty: false,
            overflowed: false,
        };
        self.send_init(id, &mut sub, src);
        self.lock().insert(id, sub);
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.lock().remove(&id);
    }

    /// Moves a window (scrolling, or opening a thread); always a fresh generation.
    pub fn update(&self, id: u64, spec: WindowSpec, src: &dyn RowSource) {
        let mut subs = self.lock();
        if let Some(sub) = subs.get_mut(&id) {
            sub.spec = clamp(spec);
            self.send_init(id, sub, src);
        }
    }

    /// The UI found a gap and asks for a fresh window.
    pub fn reset(&self, id: u64, src: &dyn RowSource) {
        let mut subs = self.lock();
        if let Some(sub) = subs.get_mut(&id) {
            self.send_init(id, sub, src);
        }
    }

    pub fn ack(&self, id: u64, generation: u64, revision: u64, src: &dyn RowSource) {
        let mut subs = self.lock();
        let Some(sub) = subs.get_mut(&id) else { return };
        if generation != sub.generation {
            return;
        }
        sub.acked = sub.acked.max(revision.min(sub.revision));
        if sub.overflowed {
            self.send_init(id, sub, src);
        } else if sub.dirty {
            self.flush_one(id, sub, src);
        }
    }

    /// Marks windows on these conversations dirty. Call [`flush`](Self::flush) to send.
    pub fn invalidate(&self, conversations: &HashSet<Uuid>) {
        for sub in self.lock().values_mut() {
            if conversations.contains(&sub.spec.conversation_id) {
                sub.dirty = true;
            }
        }
    }

    pub fn has_dirty(&self) -> bool {
        self.lock().values().any(|s| s.dirty)
    }

    pub fn flush(&self, src: &dyn RowSource) {
        let mut subs = self.lock();
        let mut gone = vec![];
        for (id, sub) in subs.iter_mut() {
            if sub.dirty && !self.flush_one(*id, sub, src) {
                gone.push(*id);
            }
        }
        for id in gone {
            subs.remove(&id);
        }
    }

    pub fn unacked(&self, id: u64) -> Option<u64> {
        self.lock().get(&id).map(|s| s.revision - s.acked)
    }

    fn send_init(&self, id: u64, sub: &mut Sub, src: &dyn RowSource) -> bool {
        let (total, views) = match src.rows(&sub.spec) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = ?e, "window rows failed");
                (0, vec![])
            }
        };
        let rows = to_rows(views);
        sub.generation += 1;
        sub.revision = 0;
        sub.acked = 0;
        sub.dirty = false;
        sub.overflowed = false;
        sub.rows = rows.iter().map(|r| (r.key, r.version)).collect();
        self.resets.fetch_add(1, Ordering::Relaxed);
        (sub.sink)(WindowBatch::Init {
            sub_id: id,
            generation: sub.generation,
            revision: 0,
            total,
            offset_from_end: sub.spec.offset_from_end,
            rows,
        })
    }

    /// Returns false when the sink is gone.
    fn flush_one(&self, id: u64, sub: &mut Sub, src: &dyn RowSource) -> bool {
        if sub.revision - sub.acked >= MAX_UNACKED {
            sub.overflowed = true;
            return true;
        }
        let (total, views) = match src.rows(&sub.spec) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = ?e, "window rows failed");
                return true;
            }
        };
        sub.dirty = false;
        let rows = to_rows(views);
        let old: HashMap<Uuid, u32> = sub.rows.iter().copied().collect();
        let new_keys: HashSet<Uuid> = rows.iter().map(|r| r.key).collect();
        let removals: Vec<Uuid> = sub
            .rows
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| !new_keys.contains(k))
            .collect();
        let new_order: Vec<Uuid> = rows.iter().map(|r| r.key).collect();
        let mut expected: Vec<Uuid> = sub
            .rows
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| new_keys.contains(k))
            .collect();
        expected.extend(rows.iter().map(|r| r.key).filter(|k| !old.contains_key(k)));
        let order = (expected != new_order).then(|| new_order.clone());
        let upserts: Vec<WindowRow> = rows
            .iter()
            .filter(|r| old.get(&r.key) != Some(&r.version))
            .cloned()
            .collect();
        if upserts.is_empty() && removals.is_empty() && order.is_none() {
            return true;
        }
        let batch = WindowBatch::Delta {
            sub_id: id,
            generation: sub.generation,
            base_revision: sub.revision,
            revision: sub.revision + 1,
            total,
            upserts,
            removals,
            order,
        };
        let size = serde_json::to_vec(&batch)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size > MAX_BATCH_BYTES {
            return self.send_init(id, sub, src);
        }
        sub.revision += 1;
        sub.rows = rows.iter().map(|r| (r.key, r.version)).collect();
        self.deltas.fetch_add(1, Ordering::Relaxed);
        (sub.sink)(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::view::SendState;
    use proto::window::{Applied, WindowClient};
    use std::sync::{Arc, Mutex as StdMutex};

    /// A conversation held in memory: rows oldest first.
    #[derive(Default)]
    struct Fake(StdMutex<Vec<MessageView>>);

    fn view(n: u128, body: &str) -> MessageView {
        MessageView {
            id: Uuid::from_u128(n),
            author_id: Uuid::nil(),
            author_name: "a".into(),
            body: body.into(),
            entities: vec![],
            created_at_ms: n as i64,
            edited: false,
            deleted: false,
            revision: 0,
            state: SendState::Sent,
            reactions: vec![],
            reply_count: 0,
            attachments: vec![],
            mine: false,
        }
    }

    impl RowSource for Fake {
        fn rows(&self, spec: &WindowSpec) -> anyhow::Result<(u32, Vec<MessageView>)> {
            let all = self.0.lock().unwrap();
            let total = all.len() as u32;
            let end = total.saturating_sub(spec.offset_from_end) as usize;
            let start = end.saturating_sub(spec.limit as usize);
            Ok((total, all[start..end].to_vec()))
        }
    }

    fn spec(limit: u32) -> WindowSpec {
        WindowSpec {
            account_id: Uuid::nil(),
            conversation_id: Uuid::nil(),
            thread_root_id: None,
            offset_from_end: 0,
            limit,
        }
    }

    fn touch(srv: &WindowServer) {
        srv.invalidate(&[Uuid::nil()].into_iter().collect());
    }

    /// Drops one delta, injects a stale-generation batch, and checks that the window resets,
    /// stays bounded, and ends with the right rows.
    #[test]
    fn dropped_and_stale_batches_reset_the_window() {
        let src = Fake::default();
        for n in 0..30 {
            src.0.lock().unwrap().push(view(n, "x"));
        }
        let srv = WindowServer::default();
        let queue: Arc<StdMutex<Vec<WindowBatch>>> = Arc::default();
        let q = queue.clone();
        let sub = srv.subscribe(
            spec(20),
            Box::new(move |b| {
                q.lock().unwrap().push(b);
                true
            }),
            &src,
        );

        let mut ui = WindowClient::new();
        let deliver = |ui: &mut WindowClient, drop_nth: Option<usize>| {
            let batches: Vec<_> = queue.lock().unwrap().drain(..).collect();
            for (i, b) in batches.into_iter().enumerate() {
                if Some(i) == drop_nth {
                    continue;
                }
                match ui.apply(b.clone()) {
                    Applied::Ack(rev) => srv.ack(sub, ui.generation, rev, &src),
                    Applied::NeedsReset => srv.reset(sub, &src),
                    Applied::Stale => {}
                }
            }
        };
        deliver(&mut ui, None);
        assert_eq!(ui.rows.len(), 20);

        // Two deltas; the first is lost in transit.
        src.0.lock().unwrap().push(view(100, "new"));
        touch(&srv);
        srv.flush(&src);
        src.0.lock().unwrap()[25] = view(25, "edited");
        touch(&srv);
        srv.flush(&src);
        let before = srv.resets.load(Ordering::Relaxed);
        deliver(&mut ui, Some(0));
        // The gap triggered a reset, which is queued now.
        assert_eq!(srv.resets.load(Ordering::Relaxed), before + 1);
        deliver(&mut ui, None);

        // A batch from an older generation is ignored.
        let stale = WindowBatch::Delta {
            sub_id: sub,
            generation: ui.generation - 1,
            base_revision: ui.revision,
            revision: ui.revision + 1,
            total: 0,
            upserts: vec![],
            removals: ui.keys(),
            order: None,
        };
        assert_eq!(ui.apply(stale), Applied::Stale);

        let expected: Vec<Uuid> = Fake::rows(&src, &spec(20))
            .unwrap()
            .1
            .iter()
            .map(|v| v.id)
            .collect();
        assert_eq!(ui.keys(), expected);
        assert_eq!(
            ui.rows
                .iter()
                .find(|r| r.key == Uuid::from_u128(25))
                .unwrap()
                .message
                .body,
            "edited"
        );
    }

    /// Without acks the core stops sending after MAX_UNACKED deltas, then resets on ack.
    #[test]
    fn unacked_batches_are_capped() {
        let src = Fake::default();
        let srv = WindowServer::default();
        let queue: Arc<StdMutex<Vec<WindowBatch>>> = Arc::default();
        let q = queue.clone();
        let sub = srv.subscribe(
            spec(50),
            Box::new(move |b| {
                q.lock().unwrap().push(b);
                true
            }),
            &src,
        );
        for n in 0..100 {
            src.0.lock().unwrap().push(view(n, "burst"));
            touch(&srv);
            srv.flush(&src);
            assert!(srv.unacked(sub).unwrap() <= MAX_UNACKED);
        }
        // One init plus at most MAX_UNACKED deltas were queued, however long the burst.
        assert_eq!(queue.lock().unwrap().len() as u64, 1 + MAX_UNACKED);
        let mut ui = WindowClient::new();
        let batches: Vec<_> = queue.lock().unwrap().drain(..).collect();
        let mut last = 0;
        for b in batches {
            if let Applied::Ack(r) = ui.apply(b) {
                last = r;
            }
        }
        srv.ack(sub, ui.generation, last, &src);
        let reset: Vec<_> = queue.lock().unwrap().drain(..).collect();
        assert_eq!(reset.len(), 1);
        assert!(matches!(reset[0], WindowBatch::Init { .. }));
        ui.apply(reset[0].clone());
        assert_eq!(ui.rows.len(), 50);
        assert_eq!(ui.rows.last().unwrap().key, Uuid::from_u128(99));
    }

    #[test]
    fn oversized_delta_becomes_reset() {
        let src = Fake::default();
        let srv = WindowServer::default();
        let queue: Arc<StdMutex<Vec<WindowBatch>>> = Arc::default();
        let q = queue.clone();
        srv.subscribe(
            spec(200),
            Box::new(move |b| {
                q.lock().unwrap().push(b);
                true
            }),
            &src,
        );
        let big = "y".repeat(4096);
        for n in 0..100 {
            src.0.lock().unwrap().push(view(n, &big));
        }
        touch(&srv);
        srv.flush(&src);
        let batches = queue.lock().unwrap();
        assert!(matches!(batches.last().unwrap(), WindowBatch::Init { .. }));
    }
}
