#![forbid(unsafe_code)]

//! Clean-room elmdb v2 two-tier live-query invalidation (`InvalidationIndex`),
//! lock-free MVCC `CommitRing` catch-up, cross-process `commit_log` selective catch-up,
//! and sans-I/O `WatchScheduler` (zero OS `Instant` calls, zero background timer threads).

pub mod index;
pub mod read_set;
pub mod scheduler;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use elmdb_engine::{CommitDeltaSummary, CompiledSchema, Engine, read_commit_seq, read_maint_seq};
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

    /// Initializes a `LiveQueryHub` pinned to `engine`'s current `(commit_seq, schema_epoch, maint_seq)`.
    pub fn from_engine<S: KvStore, V: Vfs, C: Clock>(engine: &Engine<S, V, C>, turn_slice_ms: u64) -> Result<Self> {
        let (ro, schema) = engine.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &engine.dbs())?;
        let maint_seq = read_maint_seq(&ro, &engine.dbs())?;
        let epoch = schema.epoch;
        drop(ro);
        Ok(Self {
            next_watch_id: AtomicU64::new(1),
            state: Mutex::new(HubState {
                index: InvalidationIndex::new_with_maint_seq(seq, epoch, maint_seq),
                scheduler: WatchScheduler::new(turn_slice_ms),
            }),
        })
    }

    pub fn alloc_watch_id(&self) -> WatchId {
        WatchId(self.next_watch_id.fetch_add(1, Ordering::Relaxed))
    }

    pub fn stats(&self) -> InvalidationStats {
        self.state.lock().expect("hub lock poisoned").index.stats()
    }

    /// Executes `plan` completely lock-free on a fresh MVCC snapshot, registers `watch_id`'s coalesced
    /// `ReadSet` with `CommitRing` catch-up, and seeds `WatchScheduler` as settled at virtual time `now_ms`.
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
        if self.state.lock().expect("hub lock poisoned").scheduler.is_stopped() {
            return Err(elmdb_wire::ElmdbError::new(elmdb_wire::ErrorCode::Closed, "watch scheduler is stopped"));
        }
        let watch_id = self.alloc_watch_id();
        let (read_seq, read_maint_seq, read_epoch, output, read_set) =
            run_tracked_snapshot(engine, query_engine, plan, budgets)?;
        let hash = result_hash(&output);
        let current_schema = engine.schema();

        let mut guard = self.state.lock().expect("hub lock poisoned");
        // P0-4 / P0-13: Fail closed if `stop_watches()` raced with our lock-free snapshot!
        if guard.scheduler.is_stopped() {
            return Err(elmdb_wire::ElmdbError::new(elmdb_wire::ErrorCode::Closed, "watch scheduler is stopped"));
        }
        // P0-6: Mark `already_ran = true` even when `hash == None` (e.g. Imperative watches)!
        guard.scheduler.register(watch_id, policy, now_ms, cost_ms, true, hash);
        let stale = guard.index.register_after_read_with_maint(
            watch_id,
            read_seq,
            read_maint_seq,
            read_epoch,
            &read_set,
            &current_schema,
        );
        if stale {
            guard.scheduler.mark_dirty(&[watch_id], now_ms);
        }
        Ok((watch_id, output))
    }

    /// Re-executes an existing due watch lock-free, atomically replaces its `ReadSet` with `CommitRing` catch-up,
    /// and updates `WatchScheduler`. On query error/cancellation, clears `running = false` so the scheduler never wedges!
    /// Returns `(output, should_deliver)`.
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
        {
            // Ensure `dirty -> running` transition before taking the lock-free snapshot (no-op if `next_due` already started it).
            let mut guard = self.state.lock().expect("hub lock poisoned");
            guard.scheduler.begin_run(watch_id);
        }
        let snap = match run_tracked_snapshot(engine, query_engine, plan, budgets) {
            Ok(s) => s,
            Err(e) => {
                // P0-5: Never leave `running = true` wedged on query error / cancellation!
                let mut guard = self.state.lock().expect("hub lock poisoned");
                guard.scheduler.abort_run(watch_id, true, now_ms);
                return Err(e);
            }
        };
        let (read_seq, read_maint_seq, read_epoch, output, read_set) = snap;
        let hash = result_hash(&output);
        let current_schema = engine.schema();

        let mut guard = self.state.lock().expect("hub lock poisoned");
        // P0-4: Never resurrect a watch that was unregistered or stopped while our snapshot ran!
        if guard.scheduler.is_stopped() || !guard.scheduler.contains(watch_id) {
            return Ok((output, false));
        }
        let stale = guard.index.register_after_read_with_maint(
            watch_id,
            read_seq,
            read_maint_seq,
            read_epoch,
            &read_set,
            &current_schema,
        );
        if stale {
            guard.scheduler.mark_dirty(&[watch_id], now_ms);
        }
        let should_deliver = guard.scheduler.finish_run(watch_id, now_ms, cost_ms, hash);
        Ok((output, should_deliver))
    }

    /// Single-snapshot registration: compiles, executes with `ReadSet` tracking, and materializes
    /// the watch answer inside **ONE single pinned MVCC `RoTxn`** (zero multi-snapshot TOCTOU!).
    #[allow(clippy::too_many_arguments)]
    pub fn register_query_watch_in_snapshot<S: KvStore, V: Vfs, C: Clock, A>(
        &self,
        engine: &Engine<S, V, C>,
        query_engine: &QueryEngine,
        budgets: ExecBudgets<'_>,
        policy: WatchPolicy,
        now_ms: u64,
        cost_ms: u64,
        compile_and_materialize: impl FnOnce(
            &S::RoTxn<'_>,
            &CompiledSchema,
            &mut dyn FnMut(&LogicalPlan) -> Result<QueryOutput>,
            &mut bool,
        ) -> Result<(A, Option<[u8; 16]>)>,
    ) -> Result<(WatchId, A)> {
        if self.state.lock().expect("hub lock poisoned").scheduler.is_stopped() {
            return Err(elmdb_wire::ElmdbError::new(elmdb_wire::ErrorCode::Closed, "watch scheduler is stopped"));
        }
        let watch_id = self.alloc_watch_id();
        let (ro, schema) = engine.begin_read_pinned()?;
        let read_seq = read_commit_seq(&ro, &engine.dbs())?;
        let read_maint_seq = read_maint_seq(&ro, &engine.dbs())?;
        let read_epoch = schema.epoch;
        let mut read_set = ReadSet::new();
        let mut unresolved_ext_id = false;
        let mut exec =
            |plan: &LogicalPlan| query_engine.execute_in_txn(engine, &ro, &schema, plan, budgets, &mut read_set);
        let (answer, hash) = compile_and_materialize(&ro, &schema, &mut exec, &mut unresolved_ext_id)?;
        if unresolved_ext_id {
            elmdb_query::QueryReadRecorder::record_all_entities(&mut read_set);
        }
        drop(ro);
        let current_schema = engine.schema();

        let mut guard = self.state.lock().expect("hub lock poisoned");
        if guard.scheduler.is_stopped() {
            return Err(elmdb_wire::ElmdbError::new(elmdb_wire::ErrorCode::Closed, "watch scheduler is stopped"));
        }
        guard.scheduler.register(watch_id, policy, now_ms, cost_ms, true, hash);
        let stale = guard.index.register_after_read_with_maint(
            watch_id,
            read_seq,
            read_maint_seq,
            read_epoch,
            &read_set,
            &current_schema,
        );
        if stale {
            guard.scheduler.mark_dirty(&[watch_id], now_ms);
        }
        Ok((watch_id, answer))
    }

    /// Single-snapshot rerun: compiles, executes with `ReadSet` tracking, and materializes inside
    /// **ONE single pinned MVCC `RoTxn`**, aborting `running = false` on ANY compile/exec/materialize error!
    #[allow(clippy::too_many_arguments)]
    pub fn rerun_query_watch_in_snapshot<S: KvStore, V: Vfs, C: Clock, A>(
        &self,
        watch_id: WatchId,
        engine: &Engine<S, V, C>,
        query_engine: &QueryEngine,
        budgets: ExecBudgets<'_>,
        now_ms: u64,
        cost_ms: u64,
        compile_and_materialize: impl FnOnce(
            &S::RoTxn<'_>,
            &CompiledSchema,
            &mut dyn FnMut(&LogicalPlan) -> Result<QueryOutput>,
            &mut bool,
        ) -> Result<(A, Option<[u8; 16]>)>,
    ) -> Result<(A, bool)> {
        {
            let mut guard = self.state.lock().expect("hub lock poisoned");
            guard.scheduler.begin_run(watch_id);
        }
        struct RerunSnap<A> {
            read_seq: u64,
            read_maint_seq: u64,
            read_epoch: u64,
            answer: A,
            hash: Option<[u8; 16]>,
            read_set: ReadSet,
        }
        let run_result = (|| -> Result<RerunSnap<A>> {
            let (ro, schema) = engine.begin_read_pinned()?;
            let read_seq = read_commit_seq(&ro, &engine.dbs())?;
            let read_maint_seq = read_maint_seq(&ro, &engine.dbs())?;
            let read_epoch = schema.epoch;
            let mut read_set = ReadSet::new();
            let mut unresolved_ext_id = false;
            let mut exec =
                |plan: &LogicalPlan| query_engine.execute_in_txn(engine, &ro, &schema, plan, budgets, &mut read_set);
            let (answer, hash) = compile_and_materialize(&ro, &schema, &mut exec, &mut unresolved_ext_id)?;
            if unresolved_ext_id {
                elmdb_query::QueryReadRecorder::record_all_entities(&mut read_set);
            }
            drop(ro);
            Ok(RerunSnap { read_seq, read_maint_seq, read_epoch, answer, hash, read_set })
        })();

        let RerunSnap { read_seq, read_maint_seq, read_epoch, answer, hash, read_set } = match run_result {
            Ok(v) => v,
            Err(e) => {
                let mut guard = self.state.lock().expect("hub lock poisoned");
                guard.scheduler.abort_run(watch_id, true, now_ms);
                return Err(e);
            }
        };
        let current_schema = engine.schema();

        let mut guard = self.state.lock().expect("hub lock poisoned");
        if guard.scheduler.is_stopped() || !guard.scheduler.contains(watch_id) {
            return Ok((answer, false));
        }
        let stale = guard.index.register_after_read_with_maint(
            watch_id,
            read_seq,
            read_maint_seq,
            read_epoch,
            &read_set,
            &current_schema,
        );
        if stale {
            guard.scheduler.mark_dirty(&[watch_id], now_ms);
        }
        let should_deliver = guard.scheduler.finish_run(watch_id, now_ms, cost_ms, hash);
        Ok((answer, should_deliver))
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

    #[allow(clippy::too_many_arguments)]
    pub fn notify_subdb_write_with_maint(
        &self,
        seq: u64,
        schema_epoch: u64,
        maint_seq: u64,
        slot: u16,
        key: &[u8],
        now_ms: u64,
    ) -> Vec<WatchId> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.notify_subdb_write_at_maint(seq, schema_epoch, maint_seq, slot, key);
        guard.scheduler.mark_dirty(&dirty, now_ms);
        dirty
    }

    pub fn notify_schema_change(&self, seq: u64, new_schema_epoch: u64, now_ms: u64) -> Vec<WatchId> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.notify_schema_change(seq, new_schema_epoch);
        guard.scheduler.mark_dirty(&dirty, now_ms);
        dirty
    }

    pub fn notify_schema_change_with_maint(
        &self,
        seq: u64,
        new_schema_epoch: u64,
        maint_seq: u64,
        now_ms: u64,
    ) -> Vec<WatchId> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.notify_schema_change_at_maint(seq, new_schema_epoch, maint_seq);
        guard.scheduler.mark_dirty(&dirty, now_ms);
        dirty
    }

    pub fn notify_full_rebuild_with_maint(
        &self,
        seq: u64,
        schema_epoch: u64,
        maint_seq: u64,
        now_ms: u64,
    ) -> Vec<WatchId> {
        let mut guard = self.state.lock().expect("hub lock poisoned");
        let dirty = guard.index.notify_full_rebuild_at_maint(seq, schema_epoch, maint_seq);
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

    pub fn abort_run(&self, id: WatchId, requeue_dirty: bool, now_ms: u64) -> bool {
        self.state.lock().expect("hub lock poisoned").scheduler.abort_run(id, requeue_dirty, now_ms)
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
/// `(snapshot_commit_seq, snapshot_maint_seq, snapshot_schema_epoch, QueryOutput, ReadSet)`.
pub fn run_tracked_snapshot<S: KvStore, V: Vfs, C: Clock>(
    engine: &Engine<S, V, C>,
    query_engine: &QueryEngine,
    plan: &LogicalPlan,
    budgets: ExecBudgets<'_>,
) -> Result<(u64, u64, u64, QueryOutput, ReadSet)> {
    let (ro, schema) = engine.begin_read_pinned()?;
    let read_seq = read_commit_seq(&ro, &engine.dbs())?;
    let read_maint_seq = read_maint_seq(&ro, &engine.dbs())?;
    let read_epoch = schema.epoch;
    let mut read_set = ReadSet::new();
    let output = query_engine.execute_in_txn(engine, &ro, &schema, plan, budgets, &mut read_set)?;
    drop(ro);
    Ok((read_seq, read_maint_seq, read_epoch, output, read_set))
}
