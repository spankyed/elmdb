//! Unified clean-room elmdb v2 `Database` facade composing:
//! `elmdb-lmdb` + `elmdb-engine` + `elmdb-projections` + `elmdb-vec` + `elmdb-query` + `elmdb-invalidation`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use elmdb_engine::{
    BatchOutcome, BeforeCommitHook, CompiledSchema, Engine, EntityKey, IndexKind, RawSchema, StoredEdge, StoredRow,
    TxnCtx, lookup_ext_id_in_txn, read_commit_seq, read_relations_out_in_txn, read_stored_row, row::decode_row_header,
    scan_type_keys,
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
    BatchResult, CheckReport, DatabaseStats, IndexStatus, OpenReport, QueryAnswer, QuerySpec, RelationRow, Schema,
    SchemaChange, compile_query_spec, materialize_query_answer,
};

type EngineHandle = Engine<LmdbStore, OsVfs, SystemClock>;

const BACKUP_MARKER: &str = ".elmdb-backup";

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

/// Unified embedded elmdb v2 database handle (thread-safe, MVCC lock-free reads, serialized writes).
///
/// Holding `engine` inside `RwLock<Option<Arc<EngineHandle>>>` ensures `close()` / `close_at_exit()`
/// drops the underlying `LmdbStore` (`mdb_env_close`) immediately once active operations drain,
/// releasing macOS LMDB named POSIX semaphores before V8 GC or process exit.
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
}

struct OpGuard<'a> {
    active_ops: &'a AtomicUsize,
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
        self.active_ops.fetch_sub(1, Ordering::AcqRel);
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

        let mut lmdb_cfg = LmdbConfig::default();
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
        let (engine, report) = Engine::open_with_projections(
            store,
            OsVfs,
            SystemClock::default(),
            dir.clone(),
            opts.durability,
            Arc::clone(&registry) as _,
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
        })
    }

    #[inline]
    fn enter_op(&self) -> Result<OpGuard<'_>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        }
        self.active_ops.fetch_add(1, Ordering::AcqRel);
        if self.closed.load(Ordering::Acquire) {
            self.active_ops.fetch_sub(1, Ordering::AcqRel);
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        }
        let guard = self.engine.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(engine) = guard.as_ref().map(Arc::clone) else {
            drop(guard);
            self.active_ops.fetch_sub(1, Ordering::AcqRel);
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        };
        drop(guard);
        Ok(OpGuard { active_ops: &self.active_ops, engine })
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
        drop(ro);
        self.hub.notify_schema_change(seq, compiled.epoch, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(SchemaChange::from_diff(&compiled, &diff))
    }

    pub fn rename_type(&self, old_type: &str, new_type: &str, rewrite_ids: bool) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let compiled = eng.rename_type(old_type, new_type, rewrite_ids)?;
        let (ro, _) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        drop(ro);
        self.hub.notify_schema_change(seq, compiled.epoch, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    pub fn rename_field(&self, type_name: &str, old_field: &str, new_field: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let compiled = eng.rename_field(type_name, old_field, new_field)?;
        let (ro, _) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        drop(ro);
        self.hub.notify_schema_change(seq, compiled.epoch, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    pub fn rename_relation(&self, old_rel: &str, new_rel: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let compiled = eng.rename_relation(old_rel, new_rel)?;
        let (ro, _) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        drop(ro);
        self.hub.notify_schema_change(seq, compiled.epoch, now_unix_ms());
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
        let _ = self.hub.catch_up_cross_process(&eng, now_unix_ms());
        Ok(res)
    }

    pub fn register_before_commit(&self, hook: BeforeCommitHook<LmdbStore, OsVfs, SystemClock>) -> Result<u64> {
        let eng = self.enter_op()?;
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

    pub fn get_edges(&self, ext_id: &str, rel_kind: Option<&str>, dir: EdgeDir) -> Result<Vec<StoredEdge>> {
        let eng = self.enter_op()?;
        let mut out = Vec::new();
        if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
            out.extend(eng.relations_out(ext_id, rel_kind)?);
        }
        if matches!(dir, EdgeDir::In | EdgeDir::Both) {
            out.extend(eng.relations_in(ext_id, rel_kind)?);
        }
        Ok(out)
    }

    /// Reads edges for `ext_id` and resolves all endpoint `EntityKey -> ext_id` inside one pinned MVCC snapshot.
    pub fn get_edges_resolved(&self, ext_id: &str, rel_kind: Option<&str>, dir: EdgeDir) -> Result<Vec<RelationRow>> {
        let eng = self.enter_op()?;
        let mut raw = Vec::new();
        if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
            raw.extend(eng.relations_out(ext_id, rel_kind)?);
        }
        if matches!(dir, EdgeDir::In | EdgeDir::Both) {
            raw.extend(eng.relations_in(ext_id, rel_kind)?);
        }
        let (ro, _) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let mut cache: HashMap<EntityKey, String> = HashMap::new();
        let mut out = Vec::with_capacity(raw.len());
        for e in &raw {
            let src_id = if let Some(s) = cache.get(&e.src) {
                s.clone()
            } else {
                let s = lookup_key_ext_id_in_txn(&ro, &dbs, e.src)?.unwrap_or_default();
                cache.insert(e.src, s.clone());
                s
            };
            let dst_id = if let Some(s) = cache.get(&e.dst) {
                s.clone()
            } else {
                let s = lookup_key_ext_id_in_txn(&ro, &dbs, e.dst)?.unwrap_or_default();
                cache.insert(e.dst, s.clone());
                s
            };
            out.push(RelationRow::from_stored(e, src_id, dst_id));
        }
        Ok(out)
    }

    /// Sets primary vector embeddings for `ext_id` on `index_name` (O(1) commit into `embeddings` + `vec_pending`),
    /// notifying live vector watches immediately.
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
        drop(ro);
        self.hub.notify_subdb_write_with_txnid(seq, sc.epoch, txnid, def.slot, &[0x01], now_unix_ms());
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
    // Sans-I/O Live Query & Watch Hub Methods
    // -----------------------------------------------------------------------

    pub fn register_query_watch(
        &self,
        spec: &QuerySpec,
        policy: WatchPolicy,
        now_ms: u64,
    ) -> Result<(WatchId, QueryAnswer)> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let plan = compile_query_spec(&schema, spec, |ext_id| lookup_ext_id_in_txn(&ro, &dbs, ext_id))?;
        drop(ro);

        let mut answer_out: Option<QueryAnswer> = None;
        let mut mat_err: Option<ElmdbError> = None;
        let (wid, _) = self.hub.register_query_watch(
            &eng,
            &self.query_engine,
            &plan,
            ExecBudgets::default(),
            policy,
            now_ms,
            1,
            |out| match materialize_answer_fresh(&eng, spec.explain, out) {
                Ok(ans) => {
                    let bytes = serde_json::to_vec(&ans).unwrap_or_default();
                    let h = hash_result_128(&bytes);
                    answer_out = Some(ans);
                    Some(h)
                }
                Err(e) => {
                    mat_err = Some(e);
                    None
                }
            },
        )?;
        if let Some(e) = mat_err {
            self.hub.unregister_watch(wid);
            return Err(e);
        }
        Ok((wid, answer_out.expect("materialized")))
    }

    pub fn rerun_query_watch(
        &self,
        watch_id: WatchId,
        spec: &QuerySpec,
        now_ms: u64,
        cost_ms: u64,
    ) -> Result<(QueryAnswer, bool)> {
        let eng = self.enter_op()?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let plan = compile_query_spec(&schema, spec, |ext_id| lookup_ext_id_in_txn(&ro, &dbs, ext_id))?;
        drop(ro);

        let mut answer_out: Option<QueryAnswer> = None;
        let mut mat_err: Option<ElmdbError> = None;
        let (_, should_deliver) = self.hub.rerun_query_watch(
            watch_id,
            &eng,
            &self.query_engine,
            &plan,
            ExecBudgets::default(),
            now_ms,
            cost_ms,
            |out| match materialize_answer_fresh(&eng, spec.explain, out) {
                Ok(ans) => {
                    let bytes = serde_json::to_vec(&ans).unwrap_or_default();
                    let h = hash_result_128(&bytes);
                    answer_out = Some(ans);
                    Some(h)
                }
                Err(e) => {
                    mat_err = Some(e);
                    None
                }
            },
        )?;
        if let Some(e) = mat_err {
            return Err(e);
        }
        Ok((answer_out.expect("materialized"), should_deliver))
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

    /// Drives one cooperative bounded chunk of deferred index builds, HNSW pending drains, and PageRank pushes.
    pub fn drive_background_work(&self, max_entities: usize) -> Result<bool> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        let mut more = self.registry.drive_build_chunk(&eng, max_entities.max(1))?;
        let schema = eng.schema();
        for def in &schema.indexes {
            match &def.kind {
                IndexKind::Vector { .. } => {
                    let dr = drain_pending_batch(&eng, def.id, DEFAULT_DRAIN_BATCH)?;
                    if dr.processed > 0 || dr.needs_compaction {
                        more = true;
                        if dr.needs_compaction {
                            let _ = elmdb_vec::compact_now(&eng, def.id)?;
                        }
                    }
                }
                IndexKind::Rank { .. } if rank::drain_rank_chunk(&eng, def.id, 1024)? > 0 => {
                    more = true;
                }
                _ => {}
            }
        }
        Ok(more)
    }

    /// Completes all pending deferred index builds, HNSW pending queues, PageRank push queues, and fsyncs.
    pub fn flush(&self) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        while self.registry.drive_build_chunk(&eng, 2048)? {}
        let schema = eng.schema();
        for def in &schema.indexes {
            match &def.kind {
                IndexKind::Vector { .. } => {
                    flush_vec(&eng, def.id)?;
                }
                IndexKind::Rank { .. } => {
                    rank::flush_rank(&eng, def.id)?;
                }
                _ => {}
            }
        }
        eng.sync()?;
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
        drop(ro);
        self.hub.notify_schema_change(seq, sc.epoch, now_unix_ms());
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
        let _ = self.hub.catch_up_cross_process(&eng, now_unix_ms());
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

    /// Creates a consistent live compacted backup into `target`.
    pub fn backup_into(&self, target: impl AsRef<Path>, into: BackupInto) -> Result<()> {
        let eng = self.enter_op()?;
        let target = target.as_ref();
        if target.exists() {
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
        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot create backup parent: {e}")))?;
        let tmp_name = format!(".elmdb-backup-tmp-{}-{}", std::process::id(), SystemClock::default().now_ms());
        let staging = parent.join(tmp_name);
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        eng.gate().copy_compacted(&staging)?;
        fs::write(staging.join(BACKUP_MARKER), b"elmdb-v2-backup\n")
            .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot write backup marker: {e}")))?;
        if target.exists() {
            fs::remove_dir_all(target)
                .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot replace old backup: {e}")))?;
        }
        fs::rename(&staging, target)
            .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot move backup into place: {e}")))?;
        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        self.close_within(Some(Duration::from_secs(5)))
    }

    /// Marks the database closed, waits up to `timeout` for in-flight operations to drain, syncs if writable,
    /// and immediately drops the underlying `Engine<LmdbStore>` (`mdb_env_close`) so macOS LMDB named semaphores
    /// and mmaps are released deterministically even while `Arc<Database>` is still held by JS/atexit.
    pub fn close_within(&self, timeout: Option<Duration>) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.hub.stop_watches();
        let deadline = timeout.map(|d| std::time::Instant::now() + d);
        while self.active_ops.load(Ordering::Acquire) > 0 {
            if let Some(dl) = deadline
                && std::time::Instant::now() >= dl
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let taken = self.engine.write().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(eng) = taken {
            if !self.read_only {
                let _ = eng.sync();
            }
            drop(eng);
        }
        Ok(())
    }

    pub fn close_at_exit(&self, timeout: Duration) -> Result<()> {
        self.close_within(Some(timeout))
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

fn materialize_answer_fresh(eng: &EngineHandle, include_explain: bool, out: &QueryOutput) -> Result<QueryAnswer> {
    let (ro, schema) = eng.begin_read_pinned()?;
    let dbs = eng.dbs();
    let explain = include_explain.then_some(&out.explain);
    materialize_query_answer(&schema, out.result.clone(), explain, |ek| lookup_key_ext_id_in_txn(&ro, &dbs, ek))
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
