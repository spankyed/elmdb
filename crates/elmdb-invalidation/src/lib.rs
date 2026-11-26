#![forbid(unsafe_code)]

//! Clean-room elmdb v2 two-tier live-query invalidation (`InvalidationIndex`),
//! lock-free MVCC `CommitRing` catch-up, cross-process `commit_log` selective catch-up,
//! and sans-I/O `WatchScheduler` (zero OS `Instant` calls, zero background timer threads).

pub mod index;
pub mod read_set;
pub mod scheduler;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use elmdb_engine::{CommitDeltaSummary, CompiledSchema, Engine, read_commit_seq};
use elmdb_query::{ExecBudgets, LogicalPlan, QueryEngine, QueryOutput};
use elmdb_storage::{Clock, KvStore, Vfs};
use elmdb_wire::Result;

pub use index::{COMMIT_RING_CAP, InvalidationIndex, InvalidationStats};
pub use read_set::{CoalescedRange, EdgeAdjWatch, PointWatch, ReadSet, WatchId};
pub use scheduler::{
    DEFAULT_ADAPTIVE_MULTIPLIER, DEFAULT_MIN_GAP_MS, DEFAULT_TURN_SLICE_MS, NextDue, TickResult, WatchMode,
    WatchPolicy, WatchScheduler, hash_result_128,
};

struct HubState {
    index: InvalidationIndex,
    scheduler: WatchScheduler,
}

/// Thread-safe coordinator combining `InvalidationIndex` (Tier-1 + Tier-2 + `CommitRing` + cross-process catch-up)
/// with the sans-I/O `WatchScheduler`!
///
/// Lock discipline: MVCC query execution NEVER holds `state` mutex — queries run completely lock-free
/// at snapshot `seq = S`, drop their `RoTxn`, and only briefly lock `state` to install their coalesced
/// `ReadSet` and replay `CommitRing(S + 1 ..= now)`.
pub struct LiveQueryHub {
    next_watch_id: AtomicU64,
    state: Mutex<HubState>,
}

impl Default for LiveQueryHub {
    fn default() -> Self {
        Self::new(0, 0, DEFAULT_TURN_SLICE_MS)
    }
}

impl LiveQueryHub {
    pub fn new(initial_seq: u64, initial_schema_epoch: u64, turn_slice_ms: u64) -> Self {
        Self {
            next_watch_id: AtomicU64::new(1),
            state: Mutex::new(HubState {
                index: InvalidationIndex::new(initial_seq, initial_schema_epoch),
                scheduler: WatchScheduler::new(turn_slice_ms),
            }),
        }
    }

    pub fn alloc_watch_id(&self) -> WatchId {
        WatchId(self.next_watch_id.fetch_add(1, Ordering::Relaxed))
    }

    pub fn stats(&self) -> InvalidationStats {
        self.state.lock().expect("hub lock poisoned").index.stats()
    }

    /// Executes `plan` completely lock-free on a fresh MVCC snapshot, registers `watch_id`'s coalesced
    /// `ReadSet` with `CommitRing` catch-up, and seeds `WatchScheduler` at virtual time `now_ms`.
    #[allow(clippy::too_many_arguments)]
    pub fn register_query_watch<S: KvStore, V: Vfs, C: Clock>(
        &self,
        engine: &Engine<S, V, C>,
        query_engine: &QueryEngine,
        plan: &LogicalPlan,
        budgets: ExecBudgets<'_>,
        policy: WatchPolicy,
        now_ms: u64,
        cost_ms: u64,
        result_hash: impl FnOnce(&QueryOutput) -> Option<[u8; 16]>,
    ) -> Result<(WatchId, QueryOutput)> {
        let watch_id = self.alloc_watch_id();
        let (read_seq, read_epoch, output, read_set) = run_tracked_snapshot(engine, query_engine, plan, budgets)?;
        let hash = result_hash(&output);
        let current_schema = engine.schema();

        let mut guard = self.state.lock().expect("hub lock poisoned");
        guard.scheduler.register(watch_id, policy, now_ms, cost_ms, hash);
        let stale = guard.index.register_after_read(watch_id, read_seq, read_epoch, &read_set, &current_schema);
        if stale {
            guard.scheduler.mark_dirty(&[watch_id], now_ms);
        }
        Ok((watch_id, output))
    }

    /// Re-executes an existing due watch lock-free, atomically replaces its `ReadSet` with `CommitRing` catch-up,
    /// and updates `WatchScheduler`. Returns `(output, should_deliver)`.
    #[allow(clippy::too_many_arguments)]
    pub fn rerun_query_watch<S: KvStore, V: Vfs, C: Clock>(
        &self,
        watch_id: WatchId,
        engine: &Engine<S, V, C>,
        query_engine: &QueryEngine,
        plan: &LogicalPlan,
        budgets: ExecBudgets<'_>,
        now_ms: u64,
        cost_ms: u64,
        result_hash: impl FnOnce(&QueryOutput) -> Option<[u8; 16]>,
    ) -> Result<(QueryOutput, bool)> {
        let (read_seq, read_epoch, output, read_set) = run_tracked_snapshot(engine, query_engine, plan, budgets)?;
        let hash = result_hash(&output);
        let current_schema = engine.schema();

        let mut guard = self.state.lock().expect("hub lock poisoned");
        let stale = guard.index.register_after_read(watch_id, read_seq, read_epoch, &read_set, &current_schema);
        if stale {
            guard.scheduler.mark_dirty(&[watch_id], now_ms);
        }
        let should_deliver = guard.scheduler.finish_run(watch_id, now_ms, cost_ms, hash);
        Ok((output, should_deliver))
    }

    pub fn unregister_watch(&self, watch_id: WatchId) -> bool {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let a = guard.index.unregister_watch(watch_id);
        let b = guard.scheduler.unregister(watch_id);
        a || b
    }

    pub fn notify_commit(&self, schema: &CompiledSchema, summary: &CommitDeltaSummary, now_ms: u64) -> Vec<WatchId> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.notify_commit(schema, summary);
        guard.scheduler.mark_dirty(&dirty, now_ms);
        dirty
    }

    pub fn notify_subdb_write(&self, seq: u64, schema_epoch: u64, slot: u16, key: &[u8], now_ms: u64) -> Vec<WatchId> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.notify_subdb_write(seq, schema_epoch, slot, key);
        guard.scheduler.mark_dirty(&dirty, now_ms);
        dirty
    }

    pub fn notify_schema_change(&self, seq: u64, new_schema_epoch: u64, now_ms: u64) -> Vec<WatchId> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.notify_schema_change(seq, new_schema_epoch);
        guard.scheduler.mark_dirty(&dirty, now_ms);
        dirty
    }

    pub fn catch_up_cross_process<S: KvStore, V: Vfs, C: Clock>(
        &self,
        engine: &Engine<S, V, C>,
        now_ms: u64,
    ) -> Result<Vec<WatchId>> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.catch_up_cross_process(engine)?;
        guard.scheduler.mark_dirty(&dirty, now_ms);
        Ok(dirty)
    }

    pub fn tick(&self, now_ms: u64) -> TickResult {
        self.state.lock().expect("hub lock poisoned").scheduler.tick(now_ms)
    }

    pub fn next_due(&self, now_ms: u64, elapsed_turn_ms: u64) -> NextDue {
        self.state.lock().expect("hub lock poisoned").scheduler.next_due(now_ms, elapsed_turn_ms)
    }

    pub fn begin_run(&self, id: WatchId) -> bool {
        self.state.lock().expect("hub lock poisoned").scheduler.begin_run(id)
    }

    pub fn finish_run(&self, id: WatchId, now_ms: u64, cost_ms: u64, new_hash: Option<[u8; 16]>) -> bool {
        self.state.lock().expect("hub lock poisoned").scheduler.finish_run(id, now_ms, cost_ms, new_hash)
    }

    pub fn next_wake_delay_ms(&self, now_ms: u64) -> Option<u64> {
        self.state.lock().expect("hub lock poisoned").scheduler.next_wake_delay_ms(now_ms)
    }

    pub fn caught_up(&self, now_ms: u64, soon: bool) -> bool {
        self.state.lock().expect("hub lock poisoned").scheduler.caught_up(now_ms, soon)
    }

    pub fn watches_settled(&self) -> bool {
        self.state.lock().expect("hub lock poisoned").scheduler.watches_settled()
    }

    pub fn stop_watches(&self) {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        guard.scheduler.stop_watches();
        guard.index.clear_all_watches();
    }
}

/// Helper executing `plan` lock-free inside a single MVCC `RoTxn` and returning
/// `(snapshot_commit_seq, snapshot_schema_epoch, QueryOutput, ReadSet)`.
pub fn run_tracked_snapshot<S: KvStore, V: Vfs, C: Clock>(
    engine: &Engine<S, V, C>,
    query_engine: &QueryEngine,
    plan: &LogicalPlan,
    budgets: ExecBudgets<'_>,
) -> Result<(u64, u64, QueryOutput, ReadSet)> {
    let (ro, schema) = engine.begin_read_pinned()?;
    let read_seq = read_commit_seq(&ro, &engine.dbs())?;
    let read_epoch = schema.epoch;
    let mut read_set = ReadSet::new();
    let output = query_engine.execute_in_txn(engine, &ro, &schema, plan, budgets, &mut read_set)?;
    drop(ro);
    Ok((read_seq, read_epoch, output, read_set))
}
