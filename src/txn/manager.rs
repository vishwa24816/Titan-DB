use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, TitanError};

/// Snapshot-isolated transaction context: shared snapshot timestamp.
#[derive(Debug, Clone, Copy)]
pub struct TxContext {
    pub tx_id: u64,
    pub read_ts: u64,
}

/// MVCC transaction manager: timestamp oracle + active-snapshot registry +
/// first-committer-wins validation + GC horizon.
///
/// Visibility rule: version `(begin, end)` is visible to `read_ts` iff
/// `begin <= read_ts < end` (with `end = u64::MAX` for live versions).
pub fn visible(begin: u64, end: Option<u64>, read_ts: u64) -> bool {
    begin <= read_ts && end.map(|e| read_ts < e).unwrap_or(true)
}

pub struct TxManager {
    clock: AtomicU64,
    active: parking_lot::Mutex<HashMap<u64, u64>>,
    committed: parking_lot::Mutex<HashMap<Vec<u8>, u64>>,
    writes: parking_lot::Mutex<HashMap<u64, Vec<Vec<u8>>>>,
    aborted: parking_lot::Mutex<HashSet<u64>>,
}

impl TxManager {
    pub fn new() -> Self {
        TxManager {
            clock: AtomicU64::new(1),
            active: parking_lot::Mutex::new(HashMap::new()),
            committed: parking_lot::Mutex::new(HashMap::new()),
            writes: parking_lot::Mutex::new(HashMap::new()),
            aborted: parking_lot::Mutex::new(HashSet::new()),
        }
    }

    /// Pin a snapshot and register the transaction as active.
    pub fn begin(&self) -> TxContext {
        let tx_id = self.clock.fetch_add(1, Ordering::SeqCst);
        self.active.lock().insert(tx_id, tx_id);
        self.writes.lock().insert(tx_id, Vec::new());
        TxContext { tx_id, read_ts: tx_id }
    }

    /// Tick the clock without opening a txn (compat snapshot pins).
    pub fn next_ts(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst)
    }

    /// Restore the clock after recovery: ensure future timestamps exceed
    /// every persisted version. Monotonic — never moves the clock backwards.
    pub fn restore_clock(&self, min_next: u64) {
        let mut cur = self.clock.load(Ordering::SeqCst);
        while cur < min_next {
            match self.clock.compare_exchange(cur, min_next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Record a buffered write of `key` by `tx`.
    pub fn record_write(&self, tx: u64, key: Vec<u8>) {
        if let Some(w) = self.writes.lock().get_mut(&tx) {
            w.push(key);
        }
    }

    /// First-committer-wins commit: abort with `TxConflict` if any written
    /// key was committed after our snapshot. Stamps versions on success.
    pub fn commit(&self, ctx: TxContext) -> Result<u64> {
        if self.aborted.lock().contains(&ctx.tx_id) {
            self.active.lock().remove(&ctx.tx_id);
            return Err(TitanError::TxAborted);
        }
        let keys = self.writes.lock().remove(&ctx.tx_id).unwrap_or_default();
        {
            let committed = self.committed.lock();
            for k in &keys {
                if let Some(ts) = committed.get(k) {
                    if *ts > ctx.read_ts {
                        self.active.lock().remove(&ctx.tx_id);
                        self.aborted.lock().insert(ctx.tx_id);
                        return Err(TitanError::TxConflict);
                    }
                }
            }
        }
        let commit_ts = self.clock.fetch_add(1, Ordering::SeqCst);
        {
            let mut committed = self.committed.lock();
            for k in keys {
                committed.insert(k, commit_ts);
            }
        }
        self.active.lock().remove(&ctx.tx_id);
        Ok(commit_ts)
    }

    /// Discard a transaction's buffered writes. Later use -> `TxAborted`.
    pub fn rollback(&self, ctx: TxContext) {
        self.writes.lock().remove(&ctx.tx_id);
        self.active.lock().remove(&ctx.tx_id);
        self.aborted.lock().insert(ctx.tx_id);
    }

    pub fn is_aborted(&self, tx: u64) -> bool {
        self.aborted.lock().contains(&tx)
    }

    /// GC horizon: versions with `end < horizon` are reclaimable. Never
    /// reclaims versions visible to any active snapshot.
    pub fn gc_horizon(&self) -> u64 {
        let active = self.active.lock();
        active.values().copied().min().unwrap_or(u64::MAX)
    }
}

impl Default for TxManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_first_committer_wins() {
        let m = TxManager::new();
        let t1 = m.begin();
        let t2 = m.begin();
        m.record_write(t1.tx_id, b"k".to_vec());
        m.record_write(t2.tx_id, b"k".to_vec());
        assert!(m.commit(t1).is_ok());
        assert!(matches!(m.commit(t2), Err(TitanError::TxConflict)));
    }

    #[test]
    fn tx_rollback_then_use_is_aborted() {
        let m = TxManager::new();
        let t = m.begin();
        m.record_write(t.tx_id, b"k".to_vec());
        m.rollback(t);
        assert!(m.is_aborted(t.tx_id));
        assert!(matches!(m.commit(t), Err(TitanError::TxAborted)));
    }

    #[test]
    fn tx_vacuum_horizon_below_active() {
        let m = TxManager::new();
        assert_eq!(m.gc_horizon(), u64::MAX);
        let t = m.begin();
        assert_eq!(m.gc_horizon(), t.read_ts);
    }

    #[test]
    fn tx_visibility_rule() {
        assert!(visible(5, None, 5));
        assert!(visible(5, None, 99));
        assert!(!visible(6, None, 5));
        assert!(visible(5, Some(10), 9));
        assert!(!visible(5, Some(10), 10));
    }
}
