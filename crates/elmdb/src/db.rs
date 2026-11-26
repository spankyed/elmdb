//! Unified clean-room elmdb v2 `Database` facade composing:
//! `elmdb-lmdb` + `elmdb-engine` + `elmdb-projections` + `elmdb-vec` + `elmdb-query` + `elmdb-invalidation`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use elmdb_engine::{
    BatchOutcome, BeforeCommitHook, CompiledSchema, Engine, EntityKey, IndexKind, RawSchema, StoredEdge, StoredRow,
    TxnCtx, lookup_ext_id_in_txn, read_commit_seq, read_relations_in_in_txn, read_relations_out_in_txn,
    read_stored_row, row::decode_row_header, scan_type_keys,
};
use elmdb_invalidation::{LiveQueryHub, NextDue, TickResult, WatchId, WatchPolicy, hash_result_128};
use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_projections::{BuildPolicy, ProjectionRegistry, rank};
use elmdb_query::{EdgeDir, ExecBudgets, LogicalPlan, QueryEngine, QueryOutput};
use elmdb_storage::{Clock, DurabilityMode, KvCursor, KvRead, OsVfs, SystemClock};
use elmdb_vec::{
    DEFAULT_DRAIN_BATCH, VectorProjection, drain_pending_batch, flush as flush_vec, read_entity_embeddings,
    set_entity_embeddings,
};
use elmdb_wire::{ElmdbError, EntityTarget, ErrorCode, Result, WireOp, decode_batch};

use crate::protocol::{
    BatchResult, CheckReport, DatabaseStats, IndexStatus, OpenReport, QueryAnswer, QuerySpec, RelationRow, Row, Schema,
    SchemaChange, compile_query_spec, materialize_query_answer,
};

type EngineHandle = Engine<LmdbStore, OsVfs, SystemClock>;

const BACKUP_MARKER: &str = ".elmdb-backup";

static NEXT_STAGING_NONCE: AtomicU64 = AtomicU64::new(1);

fn now_unix_ms() -> u64 {
    u64::try_from(SystemClock::default().now_ms()).unwrap_or(0)
}

/// Options for opening a `Database`.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub read_only: bool,
    pub durability: DurabilityMode,
    pub build_policy: BuildPolicy,
    pub map_size: Option<usize>,
    pub max_dbs: Option<u32>,
    pub max_readers: Option<u32>,
    pub plan_cache_capacity: usize,
    pub watch_turn_slice_ms: u64,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::DeferredWatermark,
            map_size: None,
            max_dbs: None,
            max_readers: None,
            plan_cache_capacity: elmdb_query::DEFAULT_PLAN_CACHE_CAP,
            watch_turn_slice_ms: elmdb_invalidation::DEFAULT_TURN_SLICE_MS,
        }
    }
}

/// Controls what target directories `backup_into` is permitted to overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupInto {
    /// Only into a non-existent or empty directory.
    New,
    /// Into a non-existent, empty, or previously created elmdb backup directory (`.elmdb-backup`).
    Backups,
}

#[derive(Default)]
struct CloseSync {
    done: bool,
}

/// Unified embedded elmdb v2 database handle (thread-safe, MVCC lock-free reads, serialized writes).
///
/// Holding `engine` inside `RwLock<Option<Arc<EngineHandle>>>` with a `SeqCst` Dekker handshake +
/// `Condvar` drain guarantees `close()` / `close_at_exit()` drops `LmdbStore` (`mdb_env_close`)
/// deterministically once active operations drain, releasing macOS LMDB named POSIX semaphores.
pub struct Database {
    engine: RwLock<Option<Arc<EngineHandle>>>,
    registry: Arc<ProjectionRegistry<LmdbStore, OsVfs>>,
    query_engine: QueryEngine,
    hub: LiveQueryHub,
    dir: PathBuf,
    read_only: bool,
    open_report: OpenReport,
    closed: AtomicBool,
    active_ops: AtomicUsize,
    drain_mu: Mutex<()>,
    drain_cv: Condvar,
    close_mu: Mutex<CloseSync>,
    close_cv: Condvar,
}

struct OpGuard<'a> {
    db: &'a Database,
    engine: Arc<EngineHandle>,
}

impl Deref for OpGuard<'_> {
    type Target = EngineHandle;

    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        // Release our local `Arc<EngineHandle>` BEFORE decrementing `active_ops` so that when
        // `close_within` observes `active_ops == 0`, no other thread still holds an `Arc`.
        // (We drop `self.engine` implicitly right after `drop`, so notify after decrementing
        // and let `close_within` take the `Option` under `engine.write()`.)
        let prev = self.db.active_ops.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 && self.db.closed.load(Ordering::SeqCst) {
            let _lock = self.db.drain_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.db.drain_cv.notify_all();
        }
    }
}

struct DirCleanupGuard(Option<PathBuf>);
impl DirCleanupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for DirCleanupGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

impl Database {
    /// Opens an existing database or creates a new one in `dir`.
    pub fn open(dir: impl AsRef<Path>, opts: OpenOptions) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if opts.read_only && !dir.join("data.mdb").exists() {
            return Err(ElmdbError::new(
                ErrorCode::NoDatabase,
                format!("no elmdb database found at '{}'", dir.display()),
            ));
        }

        let mut lmdb_cfg = LmdbConfig { read_only: opts.read_only, ..Default::default() };
        if let Some(ms) = opts.map_size {
            lmdb_cfg.map_size = ms;
        }
        if let Some(md) = opts.max_dbs {
            lmdb_cfg.max_dbs = md;
        }
        if let Some(mr) = opts.max_readers {
            lmdb_cfg.max_readers = mr;
        }
        let store = LmdbStore::open(&dir, lmdb_cfg)?;
        let registry = Arc::new(ProjectionRegistry::new(opts.build_policy).with_external(Arc::new(VectorProjection)));
        let (engine, report) = Engine::open_with_options(
            store,
            OsVfs,
            SystemClock::default(),
            dir.clone(),
            opts.durability,
            Arc::clone(&registry) as _,
            opts.read_only,
        )?;
        let hub = LiveQueryHub::from_engine(&engine, opts.watch_turn_slice_ms)?;

        Ok(Self {
            engine: RwLock::new(Some(Arc::new(engine))),
            registry,
            query_engine: QueryEngine::new(opts.plan_cache_capacity),
            hub,
            dir,
            read_only: opts.read_only,
            open_report: OpenReport::from(&report),
            closed: AtomicBool::new(false),
            active_ops: AtomicUsize::new(0),
            drain_mu: Mutex::new(()),
            drain_cv: Condvar::new(),
            close_mu: Mutex::new(CloseSync::default()),
            close_cv: Condvar::new(),
        })
    }

    #[inline]
    fn enter_op(&self) -> Result<OpGuard<'_>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        }
        // Hold `engine.read()` across the `active_ops` increment and `closed` re-check so
        // `close_within` (which takes `engine.write()` after setting `closed = true`) can
        // never race past an entering operation on weakly-ordered CPUs.
        let guard = self.engine.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.closed.load(Ordering::SeqCst) {
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        }
        let Some(engine) = guard.as_ref().map(Arc::clone) else {
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        };
        self.active_ops.fetch_add(1, Ordering::SeqCst);
        drop(guard);
        Ok(OpGuard { db: self, engine })
    }

    #[inline]
    fn require_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(ElmdbError::new(ErrorCode::ReadOnly, "database was opened read-only"));
        }
        Ok(())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn open_report(&self) -> &OpenReport {
        &self.open_report
    }

    pub fn acknowledge_restore(&self) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        eng.sync()?;
        Ok(())
    }

    pub fn registry(&self) -> &Arc<ProjectionRegistry<LmdbStore, OsVfs>> {
        &self.registry
    }

    pub fn hub(&self) -> &LiveQueryHub {
        &self.hub
    }

    pub fn schema(&self) -> Result<Arc<CompiledSchema>> {
        let eng = self.enter_op()?;
        Ok(eng.schema())
    }

    /// Sets or evolves the database schema (O(1) type/field renames, `Archived` types, deferred index builds).
    pub fn set_schema(&self, schema: &Schema) -> Result<SchemaChange> {
        let raw = RawSchema::from(schema);
        self.set_raw_schema(&raw)
    }

    pub fn set_raw_schema(&self, raw: &RawSchema) -> Result<SchemaChange> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let (compiled, diff) = eng.set_schema(raw)?;
        let (ro, _) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let txnid = eng.gate().last_txnid()?;
        drop(ro);
        self.hub.notify_schema_change_with_txnid(seq, compiled.epoch, txnid, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(SchemaChange::from_diff(&compiled, &diff))
    }

    pub fn rename_type(&self, old_type: &str, new_type: &str, rewrite_ids: bool) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let compiled = eng.rename_type(old_type, new_type, rewrite_ids)?;
        let (ro, _) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let txnid = eng.gate().last_txnid()?;
        drop(ro);
        self.hub.notify_schema_change_with_txnid(seq, compiled.epoch, txnid, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    pub fn rename_field(&self, type_name: &str, old_field: &str, new_field: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let compiled = eng.rename_field(type_name, old_field, new_field)?;
        let (ro, _) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let txnid = eng.gate().last_txnid()?;
        drop(ro);
        self.hub.notify_schema_change_with_txnid(seq, compiled.epoch, txnid, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    pub fn rename_relation(&self, old_rel: &str, new_rel: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let compiled = eng.rename_relation(old_rel, new_rel)?;
        let (ro, _) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let txnid = eng.gate().last_txnid()?;
        drop(ro);
        self.hub.notify_schema_change_with_txnid(seq, compiled.epoch, txnid, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    /// Executes an atomic batch of `WireOp` mutations, automatically notifying live watches on commit.
    pub fn execute_batch(&self, ops: &[WireOp]) -> Result<BatchOutcome> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let outcome = eng.execute_batch(ops)?;
        if let Some(ref sum) = outcome.summary {
            self.hub.notify_commit(&eng.schema(), sum, now_unix_ms());
        }
        Ok(outcome)
    }

    /// Decodes a binary `elmdb-wire` batch buffer, executes it atomically, and returns `BatchResult`.
    pub fn execute_wire_batch(&self, payload: &[u8]) -> Result<BatchResult> {
        let decoded = decode_batch(payload)?;
        let outcome = self.execute_batch(&decoded.ops)?;
        Ok(BatchResult {
            commit_seq: outcome.commit_seq,
            txnid: outcome.receipt.txnid,
            created_ids: outcome.created_ids,
        })
    }

    /// Runs an atomic read-write closure inside a single LMDB write transaction.
    pub fn transact<R>(
        &self,
        f: impl FnOnce(&mut TxnCtx<'_, LmdbStore, OsVfs, SystemClock>) -> Result<R>,
    ) -> Result<R> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let (res, _receipt) = eng.transact(f)?;
        self.hub.catch_up_cross_process(&eng, now_unix_ms())?;
        Ok(res)
    }

    pub fn register_before_commit(&self, hook: BeforeCommitHook<LmdbStore, OsVfs, SystemClock>) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        Ok(eng.register_before_commit(hook))
    }

    pub fn unregister_before_commit(&self, id: u64) -> bool {
        let Ok(eng) = self.enter_op() else {
            return false;
        };
        eng.unregister_before_commit(id)
    }

    pub fn get_row(&self, ext_id: &str) -> Result<Option<StoredRow>> {
        let eng = self.enter_op()?;
        eng.get_row(ext_id)
    }

    /// Reads one entity row and resolves all `EntityRef` fields inside ONE single pinned MVCC snapshot.
    pub fn get_row_resolved(&self, ext_id: &str) -> Result<Option<Row>> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let Some(ek) = lookup_ext_id_in_txn(&ro, &dbs, ext_id)? else {
            return Ok(None);
        };
        let Some(stored) = read_stored_row(&ro, &schema, &dbs, ek)? else {
            return Ok(None);
        };
        let mut cache: HashMap<EntityKey, String> = HashMap::new();
        let mut resolve_key = |k: EntityKey| -> Result<String> {
            if let Some(cached) = cache.get(&k) {
                return Ok(cached.clone());
            }
            let resolved = lookup_key_ext_id_in_txn(&ro, &dbs, k)?.unwrap_or_else(|| format!("__deleted:{}", k.0));
            cache.insert(k, resolved.clone());
            Ok(resolved)
        };
        Ok(Some(Row::from_stored_resolved(&stored, &mut resolve_key)?))
    }

    /// Reads edges for `ext_id` inside ONE single pinned MVCC snapshot.
    pub fn get_edges(&self, ext_id: &str, rel_kind: Option<&str>, dir: EdgeDir) -> Result<Vec<StoredEdge>> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let mut out = Vec::new();
        if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
            out.extend(read_relations_out_in_txn(&ro, &schema, &dbs, ext_id, rel_kind)?);
        }
        if matches!(dir, EdgeDir::In | EdgeDir::Both) {
            out.extend(read_relations_in_in_txn(&ro, &schema, &dbs, ext_id, rel_kind)?);
        }
        Ok(out)
    }

    /// Reads edges for `ext_id` and resolves all endpoint `EntityKey -> ext_id` inside ONE single pinned MVCC snapshot.
    pub fn get_edges_resolved(&self, ext_id: &str, rel_kind: Option<&str>, dir: EdgeDir) -> Result<Vec<RelationRow>> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let mut raw = Vec::new();
        if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
            raw.extend(read_relations_out_in_txn(&ro, &schema, &dbs, ext_id, rel_kind)?);
        }
        if matches!(dir, EdgeDir::In | EdgeDir::Both) {
            raw.extend(read_relations_in_in_txn(&ro, &schema, &dbs, ext_id, rel_kind)?);
        }
        let mut cache: HashMap<EntityKey, String> = HashMap::new();
        let mut out = Vec::with_capacity(raw.len());
        for e in &raw {
            let src_id = if let Some(s) = cache.get(&e.src) {
                s.clone()
            } else {
                let s = lookup_key_ext_id_in_txn(&ro, &dbs, e.src)?.unwrap_or_else(|| format!("__deleted:{}", e.src.0));
                cache.insert(e.src, s.clone());
                s
            };
            let dst_id = if let Some(s) = cache.get(&e.dst) {
                s.clone()
            } else {
                let s = lookup_key_ext_id_in_txn(&ro, &dbs, e.dst)?.unwrap_or_else(|| format!("__deleted:{}", e.dst.0));
                cache.insert(e.dst, s.clone());
                s
            };
            out.push(RelationRow::from_stored(e, src_id, dst_id));
        }
        Ok(out)
    }

    /// Sets primary vector embeddings for `ext_id` on `index_name` (O(1) commit into `embeddings` + `vec_pending`),
    /// notifying live vector watches immediately using the post-write active slot.
    pub fn set_embeddings(&self, index_name: &str, ext_id: &str, vectors: &[Vec<f32>]) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let schema = eng.schema();
        let def = schema
            .indexes
            .iter()
            .find(|i| i.name == index_name)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{index_name}'")))?;
        if !matches!(&def.kind, IndexKind::Vector { .. }) {
            return Err(ElmdbError::new(
                ErrorCode::InvalidArgument,
                format!("index '{index_name}' is not a vector index"),
            ));
        }
        let Some(row) = eng.get_row(ext_id)? else {
            return Err(ElmdbError::new(ErrorCode::EntityNotFound, format!("entity '{ext_id}' not found")));
        };
        set_entity_embeddings(&eng, def.id, row.key, vectors)?;
        let (ro, sc) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let txnid = eng.gate().last_txnid()?;
        let active_slot = sc.index_by_id(def.id).map_or(def.slot, |d| d.slot);
        drop(ro);
        self.hub.notify_subdb_write_with_txnid(seq, sc.epoch, txnid, active_slot, &[0x01], now_unix_ms());
        Ok(())
    }

    /// Executes a typed integer-ID `LogicalPlan` directly.
    pub fn execute_query(&self, plan: &LogicalPlan, budgets: ExecBudgets<'_>) -> Result<QueryOutput> {
        let eng = self.enter_op()?;
        self.query_engine.execute(&eng, plan, budgets)
    }

    /// Parses a string-named `QuerySpec` once at the boundary, executes it inside a single pinned MVCC
    /// snapshot, and materializes the external string-ID `QueryAnswer` with cached `EntityKey -> ext_id` lookups.
    pub fn execute_query_spec(&self, spec: &QuerySpec, budgets: ExecBudgets<'_>) -> Result<QueryAnswer> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let plan = compile_query_spec(&schema, spec, |ext_id| lookup_ext_id_in_txn(&ro, &dbs, ext_id))?;
        let mut noop = elmdb_query::NoopReadRecorder;
        let out = self.query_engine.execute_in_txn(&eng, &ro, &schema, &plan, budgets, &mut noop)?;
        let explain = spec.explain.then_some(&out.explain);
        materialize_query_answer(&schema, out.result, explain, |ek| lookup_key_ext_id_in_txn(&ro, &dbs, ek))
    }

    // -----------------------------------------------------------------------
    // Sans-I/O Live Query & Watch Hub Methods (Single-Snapshot Atomic)
    // -----------------------------------------------------------------------

    pub fn register_query_watch(
        &self,
        spec: &QuerySpec,
        policy: WatchPolicy,
        now_ms: u64,
    ) -> Result<(WatchId, QueryAnswer)> {
        let eng = self.enter_op()?;
        let dbs = eng.dbs();
        self.hub.register_query_watch_in_snapshot(
            &eng,
            &self.query_engine,
            ExecBudgets::default(),
            policy,
            now_ms,
            1,
            |ro, schema, exec| {
                let plan = compile_query_spec(schema, spec, |ext_id| lookup_ext_id_in_txn(ro, &dbs, ext_id))?;
                let out = exec(&plan)?;
                let explain = spec.explain.then_some(&out.explain);
                let ans =
                    materialize_query_answer(schema, out.result, explain, |ek| lookup_key_ext_id_in_txn(ro, &dbs, ek))?;
                let bytes = serde_json::to_vec(&ans)
                    .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("serialize watch answer: {e}")))?;
                let h = hash_result_128(&bytes);
                Ok((ans, Some(h)))
            },
        )
    }

    pub fn rerun_query_watch(
        &self,
        watch_id: WatchId,
        spec: &QuerySpec,
        now_ms: u64,
        cost_ms: u64,
    ) -> Result<(QueryAnswer, bool)> {
        let eng = self.enter_op()?;
        let dbs = eng.dbs();
        self.hub.rerun_query_watch_in_snapshot(
            watch_id,
            &eng,
            &self.query_engine,
            ExecBudgets::default(),
            now_ms,
            cost_ms,
            |ro, schema, exec| {
                let plan = compile_query_spec(schema, spec, |ext_id| lookup_ext_id_in_txn(ro, &dbs, ext_id))?;
                let out = exec(&plan)?;
                let explain = spec.explain.then_some(&out.explain);
                let ans =
                    materialize_query_answer(schema, out.result, explain, |ek| lookup_key_ext_id_in_txn(ro, &dbs, ek))?;
                let bytes = serde_json::to_vec(&ans)
                    .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("serialize watch answer: {e}")))?;
                let h = hash_result_128(&bytes);
                Ok((ans, Some(h)))
            },
        )
    }

    pub fn unregister_watch(&self, id: WatchId) -> bool {
        self.hub.unregister_watch(id)
    }

    pub fn catch_up_cross_process(&self, now_ms: u64) -> Result<Vec<WatchId>> {
        let eng = self.enter_op()?;
        self.hub.catch_up_cross_process(&eng, now_ms)
    }

    pub fn tick_watches(&self, now_ms: u64) -> TickResult {
        self.hub.tick(now_ms)
    }

    pub fn next_due_watch(&self, now_ms: u64, elapsed_turn_ms: u64) -> NextDue {
        self.hub.next_due(now_ms, elapsed_turn_ms)
    }

    pub fn begin_watch_run(&self, id: WatchId) -> bool {
        self.hub.begin_run(id)
    }

    pub fn abort_watch_run(&self, id: WatchId, requeue_dirty: bool, now_ms: u64) -> bool {
        self.hub.abort_run(id, requeue_dirty, now_ms)
    }

    pub fn caught_up(&self, now_ms: u64, soon: bool) -> bool {
        self.hub.caught_up(now_ms, soon)
    }

    pub fn watches_settled(&self) -> bool {
        self.hub.watches_settled()
    }

    pub fn stop_watches(&self) {
        self.hub.stop_watches();
    }

    // -----------------------------------------------------------------------
    // Maintenance, Integrity, Stats & Backup
    // -----------------------------------------------------------------------

    /// Drives one cooperative bounded chunk of deferred index builds, HNSW pending drains, and PageRank pushes,
    /// notifying `LiveQueryHub` whenever derived sub-DBs or slots change!
    pub fn drive_background_work(&self, max_entities: usize) -> Result<bool> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let mut more = false;
        let mut rebuilt_any = false;
        let mut touched_slots: Vec<u16> = Vec::new();
        let mut compacted_any = false;

        if self.registry.drive_build_chunk(&eng, max_entities.max(1))? {
            more = true;
            rebuilt_any = true;
        }
        let schema = eng.schema();
        for def in &schema.indexes {
            match &def.kind {
                IndexKind::Vector { .. } => {
                    let dr = drain_pending_batch(&eng, def.id, DEFAULT_DRAIN_BATCH)?;
                    if dr.processed > 0 || dr.needs_compaction {
                        more = true;
                        touched_slots.push(def.slot);
                        if dr.needs_compaction {
                            let _ = elmdb_vec::compact_now(&eng, def.id)?;
                            compacted_any = true;
                        }
                    }
                }
                IndexKind::Rank { .. } if rank::drain_rank_chunk(&eng, def.id, 1024)? > 0 => {
                    more = true;
                    touched_slots.push(def.slot);
                }
                _ => {}
            }
        }
        if compacted_any || rebuilt_any || !touched_slots.is_empty() {
            let (ro, sc) = eng.begin_read_pinned()?;
            let seq = read_commit_seq(&ro, &eng.dbs())?;
            let txnid = eng.gate().last_txnid()?;
            drop(ro);
            let now_ms = now_unix_ms();
            if compacted_any {
                self.hub.notify_schema_change_with_txnid(seq, sc.epoch, txnid, now_ms);
            } else if rebuilt_any {
                self.hub.notify_full_rebuild_with_txnid(seq, sc.epoch, txnid, now_ms);
            } else {
                for slot in touched_slots {
                    self.hub.notify_subdb_write_with_txnid(seq, sc.epoch, txnid, slot, &[0x01], now_ms);
                }
            }
        }
        Ok(more)
    }

    /// Completes all pending deferred index builds, HNSW pending queues, PageRank push queues, notifies watches,
    /// and fsyncs cleanly.
    pub fn flush(&self) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let mut built_any = false;
        while self.registry.drive_build_chunk(&eng, 2048)? {
            built_any = true;
        }
        let mut touched_slots = Vec::new();
        let mut compacted_any = false;
        let schema = eng.schema();
        for def in &schema.indexes {
            match &def.kind {
                IndexKind::Vector { .. } => {
                    let prev_slot = def.slot;
                    flush_vec(&eng, def.id)?;
                    touched_slots.push(prev_slot);
                    if let Some(cur) = eng.schema().index_by_id(def.id)
                        && cur.slot != prev_slot
                    {
                        compacted_any = true;
                    }
                }
                IndexKind::Rank { .. } if rank::flush_rank(&eng, def.id)? > 0 => {
                    touched_slots.push(def.slot);
                }
                _ => {}
            }
        }
        eng.sync()?;
        if compacted_any || built_any || !touched_slots.is_empty() {
            let (ro, sc) = eng.begin_read_pinned()?;
            let seq = read_commit_seq(&ro, &eng.dbs())?;
            let txnid = eng.gate().last_txnid()?;
            drop(ro);
            let now_ms = now_unix_ms();
            if compacted_any {
                self.hub.notify_schema_change_with_txnid(seq, sc.epoch, txnid, now_ms);
            } else if built_any {
                self.hub.notify_full_rebuild_with_txnid(seq, sc.epoch, txnid, now_ms);
            } else {
                for slot in touched_slots {
                    self.hub.notify_subdb_write_with_txnid(seq, sc.epoch, txnid, slot, &[0x01], now_ms);
                }
            }
        }
        Ok(())
    }

    pub fn index_status(&self) -> Result<Vec<IndexStatus>> {
        let eng = self.enter_op()?;
        let schema = eng.schema();
        let raw = self.registry.index_status(&eng)?;
        Ok(raw.iter().map(|st| IndexStatus::from_proj(&schema, st)).collect())
    }

    pub fn check(&self, index_name: Option<&str>) -> Result<CheckReport> {
        let eng = self.enter_op()?;
        let schema = eng.schema();
        let idx_id = match index_name {
            Some(name) => {
                let def = schema
                    .indexes
                    .iter()
                    .find(|i| i.name == name)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{name}'")))?;
                Some(def.id)
            }
            None => None,
        };
        let report = self.registry.check(&eng, idx_id)?;
        Ok(CheckReport::from_proj(report))
    }

    pub fn rebuild(&self, index_name: Option<&str>) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let schema = eng.schema();
        let idx_id = match index_name {
            Some(name) => {
                let def = schema
                    .indexes
                    .iter()
                    .find(|i| i.name == name)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{name}'")))?;
                Some(def.id)
            }
            None => None,
        };
        self.registry.rebuild(&eng, idx_id)?;
        let (ro, sc) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let txnid = eng.gate().last_txnid()?;
        drop(ro);
        self.hub.notify_full_rebuild_with_txnid(seq, sc.epoch, txnid, now_unix_ms());
        Ok(())
    }

    /// Destroys all entities belonging to an archived type in a single atomic transaction.
    pub fn purge_archived(&self, type_name: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let (purged, _receipt) =
            eng.transact(|tx| {
                let td =
                    tx.schema().types.iter().find(|t| t.name == type_name).ok_or_else(|| {
                        ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{type_name}'"))
                    })?;
                if !td.archived {
                    return Err(ElmdbError::new(
                        ErrorCode::InvalidArgument,
                        format!("type '{type_name}' is active; only archived types can be purged"),
                    ));
                }
                let rows = tx.list_by_type(type_name, true)?;
                let count = rows.len() as u64;
                for row in rows {
                    tx.exec(&WireOp::Destroy { target: EntityTarget::ExternalId(row.id) })?;
                }
                Ok(count)
            })?;
        self.hub.catch_up_cross_process(&eng, now_unix_ms())?;
        Ok(purged)
    }

    pub fn stats(&self) -> Result<DatabaseStats> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let commit_seq = read_commit_seq(&ro, &dbs)?;
        let txnid = eng.gate().last_txnid()?;
        let mut type_counts = BTreeMap::new();
        for td in &schema.types {
            let cnt = scan_type_keys(&ro, &dbs, td.id)?.len() as u64;
            type_counts.insert(td.name.clone(), cnt);
        }
        drop(ro);
        let inv = self.hub.stats();
        let pc = self.query_engine.cache_stats();
        let gate = eng.gate();
        Ok(DatabaseStats {
            schema_epoch: schema.epoch,
            commit_seq,
            txnid,
            type_counts,
            index_count: schema.indexes.len(),
            active_watches: inv.active_watches,
            tier2_live_ranges: inv.tier2_live_ranges,
            tier2_slab_bytes: inv.tier2_slab_bytes,
            plan_cache_hits: pc.hits,
            plan_cache_misses: pc.misses,
            map_size_bytes: gate.map_size() as u64,
            page_size_bytes: gate.page_size() as u64,
        })
    }

    /// Creates a consistent live compacted backup into `target` with a verified `synced.v2` durability sidecar,
    /// swapping atomically when replacing an existing backup so crashes never destroy the previous backup.
    pub fn backup_into(&self, target: impl AsRef<Path>, into: BackupInto) -> Result<()> {
        let eng = self.enter_op()?;
        let target = target.as_ref();
        let target_existed = target.exists();
        if target_existed {
            let mut entries = fs::read_dir(target)
                .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot read backup target: {e}")))?;
            let is_empty = entries.next().is_none();
            if !is_empty {
                match into {
                    BackupInto::New => {
                        return Err(ElmdbError::new(
                            ErrorCode::AlreadyExists,
                            format!("backup target '{}' is not empty", target.display()),
                        ));
                    }
                    BackupInto::Backups => {
                        if !target.join(BACKUP_MARKER).exists() {
                            return Err(ElmdbError::new(
                                ErrorCode::AlreadyExists,
                                format!(
                                    "refusing to overwrite non-backup directory '{}' (missing {BACKUP_MARKER})",
                                    target.display()
                                ),
                            ));
                        }
                    }
                }
            }
        }

        let parent = match target.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        fs::create_dir_all(parent)
            .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot create backup parent: {e}")))?;

        let nonce = NEXT_STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(".elmdb-backup-tmp-{}-{nonce}", std::process::id()));
        let mut cleanup = DirCleanupGuard(Some(staging.clone()));

        eng.gate().copy_compacted(&staging)?;
        fs::write(staging.join(BACKUP_MARKER), b"elmdb-v2-backup\n")
            .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot write backup marker: {e}")))?;

        if target.exists() {
            // Atomic swap: move old target aside, move staging into place (rolling back if rename fails), then delete old.
            let old_aside = parent.join(format!(".elmdb-backup-old-{}-{nonce}", std::process::id()));
            fs::rename(target, &old_aside)
                .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot stage old backup for swap: {e}")))?;
            if let Err(e) = fs::rename(&staging, target) {
                let _ = fs::rename(&old_aside, target);
                return Err(ElmdbError::new(ErrorCode::Io, format!("cannot move backup into place: {e}")));
            }
            cleanup.disarm();
            let _ = fs::remove_dir_all(&old_aside);
        } else {
            fs::rename(&staging, target)
                .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot move backup into place: {e}")))?;
            cleanup.disarm();
        }
        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        self.close_inner(Some(Duration::from_secs(5)), true)
    }

    pub fn close_within(&self, timeout: Option<Duration>) -> Result<()> {
        self.close_inner(timeout, true)
    }

    pub fn close_at_exit(&self, timeout: Duration) -> Result<()> {
        self.close_inner(Some(timeout), false)
    }

    /// Marks the database closed, waits for active operations to drain (using `Condvar` + `SeqCst` handshake),
    /// syncs if drained & writable, and immediately drops the underlying `Engine<LmdbStore>` (`mdb_env_close`).
    /// Concurrent `close()` callers wait until closing finishes instead of returning prematurely.
    fn close_inner(&self, timeout: Option<Duration>, propagate_sync_err: bool) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            let mut st = self.close_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while !st.done {
                st = self.close_cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            return Ok(());
        }

        struct MarkCloseDone<'a>(&'a Database);
        impl Drop for MarkCloseDone<'_> {
            fn drop(&mut self) {
                let mut st = self.0.close_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                st.done = true;
                self.0.close_cv.notify_all();
            }
        }
        let _mark_done = MarkCloseDone(self);

        self.hub.stop_watches();

        // Take the engine handle out under `engine.write()`, which acts as a full barrier against
        // any concurrent `enter_op` acquiring `engine.read()` + incrementing `active_ops`.
        let taken = self.engine.write().unwrap_or_else(std::sync::PoisonError::into_inner).take();

        // Wait for already-entered operations to drain cleanly via `drain_cv`.
        let drained = {
            let mut lock = self.drain_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let deadline = timeout.map(|d| std::time::Instant::now() + d);
            loop {
                if self.active_ops.load(Ordering::SeqCst) == 0 {
                    break true;
                }
                match deadline {
                    Some(dl) => {
                        let now = std::time::Instant::now();
                        if now >= dl {
                            break false;
                        }
                        let (next_lock, _) = self
                            .drain_cv
                            .wait_timeout(lock, dl.saturating_duration_since(now))
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        lock = next_lock;
                    }
                    None => {
                        lock = self.drain_cv.wait(lock).unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
            }
        };

        let mut sync_res = Ok(());
        if let Some(eng) = taken {
            if !self.read_only && drained {
                sync_res = eng.sync();
            }
            drop(eng);
        }
        if propagate_sync_err { sync_res } else { Ok(()) }
    }

    /// Single-snapshot scan of schema, primary rows, outgoing edges, and embeddings (used by `dump`).
    pub(crate) fn snapshot_for_dump(&self) -> Result<DumpSnapshot> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();

        let mut rows = Vec::new();
        {
            let mut cur = ro.cursor(dbs.entities)?;
            let mut item = cur.first()?;
            while let Some((k, _)) = item {
                if k.len() == 8 {
                    let ek = EntityKey::from_ne_bytes(k.try_into().expect("8"));
                    if let Some(row) = read_stored_row(&ro, &schema, &dbs, ek)? {
                        rows.push(row);
                    }
                }
                item = cur.next()?;
            }
        }

        let mut edges = Vec::new();
        for r in &rows {
            edges.extend(read_relations_out_in_txn(&ro, &schema, &dbs, &r.id, None)?);
        }

        let mut embeddings = Vec::new();
        for def in &schema.indexes {
            if let IndexKind::Vector { dims, .. } = &def.kind {
                for r in &rows {
                    if r.type_id != def.type_id {
                        continue;
                    }
                    let vecs = read_entity_embeddings(&ro, &dbs, def.id, r.key, *dims as usize)?;
                    if !vecs.is_empty() {
                        embeddings.push((def.name.clone(), r.id.clone(), vecs));
                    }
                }
            }
        }
        Ok(DumpSnapshot { schema, rows, edges, embeddings })
    }
}

pub(crate) struct DumpSnapshot {
    pub schema: Arc<CompiledSchema>,
    pub rows: Vec<StoredRow>,
    pub edges: Vec<StoredEdge>,
    pub embeddings: Vec<(String, String, Vec<Vec<f32>>)>,
}

fn lookup_key_ext_id_in_txn<R: KvRead>(
    ro: &R,
    dbs: &elmdb_engine::SystemDbs,
    key: EntityKey,
) -> Result<Option<String>> {
    let Some(raw) = ro.get(dbs.entities, &key.to_ne_bytes())? else {
        return Ok(None);
    };
    let (hdr, _) = decode_row_header(raw)?;
    Ok(Some(hdr.ext_id))
}
