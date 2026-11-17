//! Windowed subscriptions (decision 4). The core owns the rows; the UI holds a bounded window.
//!
//! The UI receives an `Init` and then batched `Delta`s. A delta applies only on top of the
//! revision it names; anything else is a gap, and the UI asks for a reset.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::view::MessageView;

/// Most rows a window may hold.
pub const MAX_WINDOW_ROWS: u32 = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSpec {
    pub account_id: Uuid,
    pub conversation_id: Uuid,
    /// `Some(root)` for a thread pane: the root followed by its replies.
    pub thread_root_id: Option<Uuid>,
    /// Rows counted back from the newest row; 0 means "pinned to the bottom".
    pub offset_from_end: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRow {
    pub key: Uuid,
    /// Changes whenever the rendered content of the row changes. 32 bits so it survives
    /// the trip through JavaScript numbers.
    pub version: u32,
    pub message: MessageView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WindowBatch {
    Init {
        sub_id: u64,
        generation: u64,
        revision: u64,
        /// Rows in the whole conversation, for scroll extent.
        total: u32,
        offset_from_end: u32,
        rows: Vec<WindowRow>,
    },
    Delta {
        sub_id: u64,
        generation: u64,
        base_revision: u64,
        revision: u64,
        total: u32,
        upserts: Vec<WindowRow>,
        removals: Vec<Uuid>,
        /// Full key order when it changed; at most `MAX_WINDOW_ROWS` keys.
        order: Option<Vec<Uuid>>,
    },
}

impl WindowBatch {
    pub fn sub_id(&self) -> u64 {
        match self {
            WindowBatch::Init { sub_id, .. } | WindowBatch::Delta { sub_id, .. } => *sub_id,
        }
    }
    pub fn generation(&self) -> u64 {
        match self {
            WindowBatch::Init { generation, .. } | WindowBatch::Delta { generation, .. } => {
                *generation
            }
        }
    }
    pub fn revision(&self) -> u64 {
        match self {
            WindowBatch::Init { revision, .. } | WindowBatch::Delta { revision, .. } => *revision,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Applied; acknowledge this revision.
    Ack(u64),
    /// From an older generation; dropped.
    Stale,
    /// A gap or malformed batch; the window was cleared and must be reset.
    NeedsReset,
}

/// UI-side state of one window. Pure, so it is unit-tested natively and runs in WASM.
#[derive(Debug, Clone, Default)]
pub struct WindowClient {
    pub generation: u64,
    pub revision: u64,
    pub total: u32,
    pub offset_from_end: u32,
    pub rows: Vec<WindowRow>,
    awaiting_reset: bool,
    initialised: bool,
}

impl WindowClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, batch: WindowBatch) -> Applied {
        match batch {
            WindowBatch::Init {
                generation,
                revision,
                total,
                offset_from_end,
                mut rows,
                ..
            } => {
                if self.initialised && generation < self.generation {
                    return Applied::Stale;
                }
                rows.truncate(MAX_WINDOW_ROWS as usize);
                self.generation = generation;
                self.revision = revision;
                self.total = total;
                self.offset_from_end = offset_from_end;
                self.rows = rows;
                self.awaiting_reset = false;
                self.initialised = true;
                Applied::Ack(revision)
            }
            WindowBatch::Delta {
                generation,
                base_revision,
                revision,
                total,
                upserts,
                removals,
                order,
                ..
            } => {
                if !self.initialised || generation < self.generation {
                    return Applied::Stale;
                }
                if self.awaiting_reset {
                    return Applied::Stale;
                }
                if generation > self.generation || base_revision != self.revision {
                    return self.gap();
                }
                for key in &removals {
                    self.rows.retain(|r| r.key != *key);
                }
                for row in upserts {
                    match self.rows.iter_mut().find(|r| r.key == row.key) {
                        Some(existing) => *existing = row,
                        None => self.rows.push(row),
                    }
                }
                if let Some(order) = order {
                    if order.len() > MAX_WINDOW_ROWS as usize {
                        return self.gap();
                    }
                    let mut reordered = Vec::with_capacity(order.len());
                    for key in &order {
                        match self.rows.iter().position(|r| r.key == *key) {
                            Some(i) => reordered.push(self.rows.swap_remove(i)),
                            None => return self.gap(),
                        }
                    }
                    self.rows = reordered;
                }
                if self.rows.len() > MAX_WINDOW_ROWS as usize {
                    return self.gap();
                }
                self.total = total;
                self.revision = revision;
                Applied::Ack(revision)
            }
        }
    }

    fn gap(&mut self) -> Applied {
        self.awaiting_reset = true;
        Applied::NeedsReset
    }

    pub fn keys(&self) -> Vec<Uuid> {
        self.rows.iter().map(|r| r.key).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::SendState;

    fn row(n: u128, v: u32) -> WindowRow {
        WindowRow {
            key: Uuid::from_u128(n),
            version: v,
            message: MessageView {
                id: Uuid::from_u128(n),
                author_id: Uuid::nil(),
                author_name: "a".into(),
                body: format!("m{n} v{v}"),
                entities: vec![],
                created_at_ms: 0,
                edited: false,
                deleted: false,
                revision: 0,
                state: SendState::Sent,
                reactions: vec![],
                reply_count: 0,
                attachments: vec![],
                mine: false,
            },
        }
    }

    fn init(generation: u64, rows: Vec<WindowRow>) -> WindowBatch {
        WindowBatch::Init {
            sub_id: 1,
            generation,
            revision: 0,
            total: rows.len() as u32,
            offset_from_end: 0,
            rows,
        }
    }

    fn delta(generation: u64, base: u64, upserts: Vec<WindowRow>) -> WindowBatch {
        let order = None;
        WindowBatch::Delta {
            sub_id: 1,
            generation,
            base_revision: base,
            revision: base + 1,
            total: 0,
            upserts,
            removals: vec![],
            order,
        }
    }

    #[test]
    fn applies_in_order_and_detects_gap() {
        let mut w = WindowClient::new();
        assert_eq!(w.apply(init(1, vec![row(1, 0)])), Applied::Ack(0));
        assert_eq!(w.apply(delta(1, 0, vec![row(2, 0)])), Applied::Ack(1));
        // Revision 2 is lost; 3 arrives on base 2.
        assert_eq!(w.apply(delta(1, 2, vec![row(3, 0)])), Applied::NeedsReset);
        // Further deltas are ignored until the reset arrives.
        assert_eq!(w.apply(delta(1, 3, vec![row(4, 0)])), Applied::Stale);
        assert_eq!(
            w.apply(init(2, vec![row(1, 0), row(2, 0)])),
            Applied::Ack(0)
        );
        assert_eq!(w.keys(), vec![Uuid::from_u128(1), Uuid::from_u128(2)]);
    }

    #[test]
    fn stale_generation_is_dropped() {
        let mut w = WindowClient::new();
        w.apply(init(3, vec![row(1, 0)]));
        assert_eq!(w.apply(delta(2, 0, vec![row(9, 0)])), Applied::Stale);
        assert_eq!(w.apply(init(2, vec![row(9, 0)])), Applied::Stale);
        assert_eq!(w.keys(), vec![Uuid::from_u128(1)]);
    }

    #[test]
    fn rejects_oversized_window() {
        let mut w = WindowClient::new();
        w.apply(init(1, vec![]));
        let rows = (0..(MAX_WINDOW_ROWS as u128 + 1))
            .map(|n| row(n, 0))
            .collect();
        assert_eq!(w.apply(delta(1, 0, rows)), Applied::NeedsReset);
    }
}
