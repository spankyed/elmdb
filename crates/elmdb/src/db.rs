//! Unified clean-room elmdb v2 `Database` facade composing:
//! `elmdb-lmdb` + `elmdb-engine` + `elmdb-projections` + `elmdb-vec` + `elmdb-query` + `elmdb-invalidation`.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use elmdb_engine::{
    BatchOutcome, BeforeCommitHook, CompiledSchema, Engine, EntityKey, HookRoundSnapshot, IndexId, IndexKind,
    MAX_HOOK_ROUNDS, RawSchema, StoredEdge, StoredRow, TxnCtx, TypeMask, list_counters_in_txn, lookup_ext_id_in_txn,
    read_commit_seq, read_maint_seq, read_relations_in_in_txn, read_relations_out_in_txn, read_stored_row,
    row::decode_row_header, scan_type_keys,
};
use elmdb_invalidation::{LiveQueryHub, NextDue, TickResult, WatchId, WatchPolicy, hash_result_128};
use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_projections::{BuildPolicy, ProjectionRegistry, rank};
use elmdb_query::{EdgeDir, ExecBudgets, LogicalPlan, QueryEngine, QueryOutput};
use elmdb_storage::{Clock, DurabilityMode, KvCursor, KvRead, OsVfs, SystemClock};
use elmdb_vec::{DEFAULT_DRAIN_BATCH, VectorProjection, drain_pending_batch_with_receipt, read_entity_embeddings};
use elmdb_wire::{ElmdbError, EntityTarget, ErrorCode, Result, WireOp, decode_batch};

use crate::protocol::{
    BatchResult, CheckReport, DatabaseStats, IndexStatus, OpenReport, QueryAnswer, QuerySpec, RelationRow, Row, Schema,
    SchemaChange, compile_query_spec, materialize_query_answer,
};

type EngineHandle = Engine<LmdbStore, OsVfs, SystemClock>;

const BACKUP_MARKER: &str = ".elmdb-backup";

static NEXT_STAGING_NONCE: AtomicU64 = AtomicU64::new(1);
static NEXT_DB_ID: AtomicU64 = AtomicU64::new(1);
/// High bit tag distinguishing host `beforeCommit` hook IDs from engine Rust hook IDs.
/// Kept at bit 48 so every hook ID stays exact within IEEE-754 JS safe integers (`< 2^53`).
const HOST_HOOK_ID_FLAG: u64 = 1u64 << 48;

/// Options controlling `Database::transact_scoped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactOptions {
    /// Whether `beforeCommit` hooks fire on the outermost commit (default `true`).
    /// Pass `false` for projection catch-up chunks that must bypass hooks.
    pub run_before_commit: bool,
}

impl Default for TransactOptions {
    fn default() -> Self {
        Self { run_before_commit: true }
    }
}

/// Entity delta record delivered to host/JS `beforeCommit` hooks.
#[derive(Debug, Clone)]
pub struct HostEntityDelta {
    pub id: String,
    pub type_name: String,
    pub op: &'static str,
}

/// Edge delta record delivered to host/JS `beforeCommit` hooks.
#[derive(Debug, Clone)]
pub struct HostEdgeDelta {
    pub rel_kind: String,
    pub op: &'static str,
    pub source_id: String,
    pub target_id: String,
}

/// Per-round delta payload delivered to host/JS `beforeCommit` hooks (with pending `commit_seq`).
#[derive(Debug, Clone)]
pub struct HostHookDelta {
    pub commit_seq: u64,
    pub entities: Vec<HostEntityDelta>,
    pub edges: Vec<HostEdgeDelta>,
}

/// Host/JS synchronous `beforeCommit` callback invoked while the active transaction savepoint is parked
/// on the calling thread, allowing full RYOW `get`, `query`, `next_counter`, and `pending_commit_seq`.
pub type HostBeforeCommitFn = Arc<dyn Fn(&HostHookDelta) -> Result<Option<Vec<u8>>> + Send + Sync>;

struct RegisteredHostHook {
    id: u64,
    wants: TypeMask,
    callback: HostBeforeCommitFn,
}

struct ActiveTxnSlot {
    ctx: Option<TxnCtx<'static, LmdbStore, OsVfs, SystemClock>>,
    commit_seq: Option<u64>,
    depth: usize,
}

thread_local! {
    static ACTIVE_TXNS: RefCell<HashMap<u64, ActiveTxnSlot>> = RefCell::new(HashMap::new());
}

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
    outcome: Option<(ErrorCode, String)>,
}

/// Unified embedded elmdb v2 database handle (thread-safe, MVCC lock-free reads, serialized writes).
///
/// Holding `engine` inside `RwLock<Option<Arc<EngineHandle>>>` with a `SeqCst` Dekker handshake +
/// `Condvar` drain guarantees `close()` / `close_at_exit()` drops `LmdbStore` (`mdb_env_close`)
/// deterministically once active operations drain, releasing macOS LMDB named POSIX semaphores.
pub struct Database {
    id: u64,
    engine: RwLock<Option<Arc<EngineHandle>>>,
    registry: Arc<ProjectionRegistry<LmdbStore, OsVfs>>,
    query_engine: QueryEngine,
    hub: LiveQueryHub,
    host_hooks: Mutex<Vec<RegisteredHostHook>>,
    next_host_hook_id: AtomicU64,
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
    engine: Option<Arc<EngineHandle>>,
}

impl OpGuard<'_> {
    fn arc(&self) -> &Arc<EngineHandle> {
        self.engine.as_ref().expect("OpGuard engine live until Drop")
    }
}

impl Deref for OpGuard<'_> {
    type Target = EngineHandle;

    fn deref(&self) -> &Self::Target {
        self.engine.as_ref().expect("OpGuard engine live until Drop")
    }
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        // Drop our local `Arc<EngineHandle>` strictly BEFORE decrementing `active_ops` so that when
        // `close_inner` observes `active_ops == 0`, zero other threads can still hold an `Arc` and
        // `mdb_env_close()` is guaranteed to complete before `close()` returns!
        drop(self.engine.take());
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

        let mut lmdb_cfg = LmdbConfig {
            read_only: opts.read_only,
            no_sync: opts.durability != DurabilityMode::Strict,
            ..Default::default()
        };
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
            OsVfs::with_boot_id(elmdb_lmdb::platform_boot_id()),
            SystemClock::default(),
            dir.clone(),
            opts.durability,
            Arc::clone(&registry) as _,
            opts.read_only,
        )?;
        let hub = LiveQueryHub::from_engine(&engine, opts.watch_turn_slice_ms)?;

        Ok(Self {
            id: NEXT_DB_ID.fetch_add(1, Ordering::Relaxed),
            engine: RwLock::new(Some(Arc::new(engine))),
            registry,
            query_engine: QueryEngine::new(opts.plan_cache_capacity),
            hub,
            host_hooks: Mutex::new(Vec::new()),
            next_host_hook_id: AtomicU64::new(1),
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
        Ok(OpGuard { db: self, engine: Some(engine) })
    }

    #[inline]
    fn require_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(ElmdbError::new(ErrorCode::ReadOnly, "database was opened read-only"));
        }
        Ok(())
    }

    #[inline]
    fn has_active_txn(&self) -> bool {
        ACTIVE_TXNS.try_with(|m| m.try_borrow().map_or(true, |map| map.contains_key(&self.id))).unwrap_or(false)
    }

    #[inline]
    fn forbid_inside_txn(&self, op_name: &str) -> Result<()> {
        if self.has_active_txn() {
            return Err(ElmdbError::new(
                ErrorCode::InvalidArgument,
                format!("'{op_name}' cannot be called inside an active transact() or beforeCommit hook"),
            ));
        }
        Ok(())
    }

    fn with_active_ctx<R>(
        &self,
        f: impl FnOnce(&TxnCtx<'static, LmdbStore, OsVfs, SystemClock>) -> Result<R>,
    ) -> Result<R> {
        ACTIVE_TXNS.with(|m| {
            let map = m.try_borrow().map_err(|_| {
                ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    "cannot call Database methods re-entrantly inside a Rust beforeCommit hook; use the TxnCtx argument instead",
                )
            })?;
            let slot = map
                .get(&self.id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Internal, "expected active transaction on calling thread"))?;
            let ctx = slot.ctx.as_ref().ok_or_else(|| {
                ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    "cannot call Database methods re-entrantly while a Rust beforeCommit hook owns &mut TxnCtx; use the TxnCtx argument instead",
                )
            })?;
            f(ctx)
        })
    }

    fn with_active_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut TxnCtx<'static, LmdbStore, OsVfs, SystemClock>) -> Result<R>,
    ) -> Result<R> {
        ACTIVE_TXNS.with(|m| {
            let mut map = m.try_borrow_mut().map_err(|_| {
                ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    "cannot call Database methods re-entrantly inside a Rust beforeCommit hook; use the TxnCtx argument instead",
                )
            })?;
            let slot = map
                .get_mut(&self.id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Internal, "expected active transaction on calling thread"))?;
            let ctx = slot.ctx.as_mut().ok_or_else(|| {
                ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    "cannot call Database methods re-entrantly while a Rust beforeCommit hook owns &mut TxnCtx; use the TxnCtx argument instead",
                )
            })?;
            f(ctx)
        })
    }

    /// Returns the pending monotonic commit sequence number if currently inside `transact()` or `beforeCommit`,
    /// or `None` outside an active write transaction.
    pub fn pending_commit_seq(&self) -> Option<u64> {
        ACTIVE_TXNS
            .try_with(|m| m.try_borrow().ok().and_then(|map| map.get(&self.id).and_then(|slot| slot.commit_seq)))
            .ok()
            .flatten()
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
        self.forbid_inside_txn("acknowledgeRestore")?;
        eng.acknowledge_restore()?;
        Ok(())
    }

    pub fn registry(&self) -> &Arc<ProjectionRegistry<LmdbStore, OsVfs>> {
        &self.registry
    }

    pub fn hub(&self) -> &LiveQueryHub {
        &self.hub
    }

    fn fresh_schema(&self, eng: &EngineHandle) -> Result<Arc<CompiledSchema>> {
        let (_ro, schema) = eng.begin_read_pinned()?;
        Ok(schema)
    }

    pub fn schema(&self) -> Result<Arc<CompiledSchema>> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| Ok(Arc::clone(ctx.schema())));
        }
        self.fresh_schema(&eng)
    }

    /// Sets or evolves the database schema (O(1) type/field renames, `Archived` types, deferred index builds).
    pub fn set_schema(&self, schema: &Schema) -> Result<SchemaChange> {
        let raw = RawSchema::from(schema);
        self.set_raw_schema(&raw)
    }

    pub fn set_raw_schema(&self, raw: &RawSchema) -> Result<SchemaChange> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("setSchema")?;
        let (compiled, diff) = eng.set_schema(raw)?;
        let (ro, ro_schema) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let maint_seq = read_maint_seq(&ro, &eng.dbs())?;
        drop(ro);
        self.hub.notify_schema_change_with_maint(seq, ro_schema.epoch, maint_seq, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(SchemaChange::from_diff(&compiled, &diff))
    }

    pub fn rename_type(&self, old_type: &str, new_type: &str, rewrite_ids: bool) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("renameType")?;
        let compiled = eng.rename_type(old_type, new_type, rewrite_ids)?;
        let (ro, ro_schema) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let maint_seq = read_maint_seq(&ro, &eng.dbs())?;
        drop(ro);
        self.hub.notify_schema_change_with_maint(seq, ro_schema.epoch, maint_seq, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    pub fn rename_field(&self, type_name: &str, old_field: &str, new_field: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("renameField")?;
        let compiled = eng.rename_field(type_name, old_field, new_field)?;
        let (ro, ro_schema) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let maint_seq = read_maint_seq(&ro, &eng.dbs())?;
        drop(ro);
        self.hub.notify_schema_change_with_maint(seq, ro_schema.epoch, maint_seq, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    pub fn rename_relation(&self, old_rel: &str, new_rel: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("renameRelation")?;
        let compiled = eng.rename_relation(old_rel, new_rel)?;
        let (ro, ro_schema) = eng.begin_read_pinned()?;
        let seq = read_commit_seq(&ro, &eng.dbs())?;
        let maint_seq = read_maint_seq(&ro, &eng.dbs())?;
        drop(ro);
        self.hub.notify_schema_change_with_maint(seq, ro_schema.epoch, maint_seq, now_unix_ms());
        self.query_engine.clear_cache();
        Ok(compiled.epoch)
    }

    /// Runs a strictly-synchronous callback inside an atomic LMDB write transaction with native nested savepoints.
    ///
    /// - Outer `transact_scoped`: opens an LMDB `RwTxn`, runs `f()`, runs `beforeCommit` hooks ONCE on commit
    ///   (unless `opts.run_before_commit == false`), commits atomically, and notifies `LiveQueryHub` once.
    /// - Nested `transact_scoped` (called re-entrantly on the same thread & database): opens a native LMDB
    ///   savepoint; returning `Err` rolls back ONLY the inner savepoint while outer writes stay visible via RYOW.
    ///   `beforeCommit` never fires on inner savepoint release.
    pub fn transact_scoped<R>(&self, opts: TransactOptions, f: impl FnOnce() -> Result<R>) -> Result<R> {
        Ok(self.transact_scoped_with_receipt(opts, f)?.0)
    }

    fn transact_scoped_with_receipt<R>(
        &self,
        opts: TransactOptions,
        f: impl FnOnce() -> Result<R>,
    ) -> Result<(R, Option<(u64, elmdb_storage::CommitReceipt)>)> {
        let eng = self.enter_op()?;
        self.require_writable()?;

        if self.has_active_txn() {
            let sp_depth = ACTIVE_TXNS.with(|m| -> Result<u32> {
                let mut map = m.try_borrow_mut().map_err(|_| {
                    ElmdbError::new(
                        ErrorCode::InvalidArgument,
                        "cannot nest transact() inside a Rust beforeCommit hook; use the TxnCtx argument instead",
                    )
                })?;
                let slot = map
                    .get_mut(&self.id)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Internal, "expected active transaction"))?;
                let ctx = slot.ctx.as_mut().ok_or_else(|| {
                    ElmdbError::new(
                        ErrorCode::InvalidArgument,
                        "cannot nest transact() while a Rust beforeCommit hook owns &mut TxnCtx; use the TxnCtx argument instead",
                    )
                })?;
                let depth = ctx.push_savepoint()?;
                slot.depth += 1;
                Ok(depth)
            })?;

            struct NestedGuard {
                db_id: u64,
                sp_depth: u32,
                armed: bool,
            }
            impl Drop for NestedGuard {
                fn drop(&mut self) {
                    if self.armed {
                        let _ = ACTIVE_TXNS.try_with(|m| {
                            if let Ok(mut map) = m.try_borrow_mut()
                                && let Some(slot) = map.get_mut(&self.db_id)
                            {
                                if let Some(ctx) = slot.ctx.as_mut() {
                                    ctx.abort_savepoint(self.sp_depth);
                                }
                                slot.depth = slot.depth.saturating_sub(1);
                            }
                        });
                    }
                }
            }

            let mut guard = NestedGuard { db_id: self.id, sp_depth, armed: true };
            let res = f();
            guard.armed = false;
            return match res {
                Ok(val) => {
                    ACTIVE_TXNS.with(|m| -> Result<()> {
                        let mut map = m.borrow_mut();
                        let slot = map.get_mut(&self.id).expect("active txn");
                        let commit_res = match slot.ctx.as_mut() {
                            Some(ctx) => ctx.commit_savepoint(sp_depth),
                            None => {
                                Err(ElmdbError::new(ErrorCode::Internal, "active txn ctx missing on savepoint commit"))
                            }
                        };
                        slot.depth = slot.depth.saturating_sub(1);
                        commit_res
                    })?;
                    Ok((val, None))
                }
                Err(err) => {
                    ACTIVE_TXNS.with(|m| {
                        let mut map = m.borrow_mut();
                        if let Some(slot) = map.get_mut(&self.id) {
                            if let Some(ctx) = slot.ctx.as_mut() {
                                ctx.abort_savepoint(sp_depth);
                            }
                            slot.depth = slot.depth.saturating_sub(1);
                        }
                    });
                    Err(err)
                }
            };
        }

        // Outermost transaction on this thread: capture `map_size` right after `begin_owned_txn()`
        // succeeds (capturing any proactive map growth) so if this attempt aborts with `MDB_MAP_FULL`,
        // concurrent threads target the exact overflowed `before * 2` idempotently after `OuterGuard`
        // drops and releases L2 + L3.
        let mut map_size_before = eng.gate().map_size();
        let result = (|| -> Result<(R, Option<(u64, elmdb_storage::CommitReceipt)>)> {
            let ctx = eng.arc().begin_owned_txn()?;
            map_size_before = eng.gate().map_size();
            let commit_seq = ctx.commit_seq();
            ACTIVE_TXNS.with(|m| {
                m.borrow_mut()
                    .insert(self.id, ActiveTxnSlot { ctx: Some(ctx), commit_seq: Some(commit_seq), depth: 1 });
            });

            struct OuterGuard {
                db_id: u64,
                armed: bool,
            }
            impl Drop for OuterGuard {
                fn drop(&mut self) {
                    if self.armed {
                        let _ = ACTIVE_TXNS.try_with(|m| {
                            if let Ok(mut map) = m.try_borrow_mut() {
                                // Dropping `ActiveTxnSlot` aborts the root LMDB RwTxn and releases WriterLock.
                                let _ = map.remove(&self.db_id);
                            }
                        });
                    }
                }
            }

            let mut guard = OuterGuard { db_id: self.id, armed: true };
            let value = f()?;
            if opts.run_before_commit {
                self.run_combined_hook_cascade(&eng)?;
            }
            guard.armed = false;
            let slot = ACTIVE_TXNS
                .with(|m| m.borrow_mut().remove(&self.id))
                .ok_or_else(|| ElmdbError::new(ErrorCode::Internal, "active txn slot missing on commit"))?;
            let ctx =
                slot.ctx.ok_or_else(|| ElmdbError::new(ErrorCode::Internal, "active txn ctx missing on commit"))?;
            let committed_schema = Arc::clone(ctx.schema());
            let committed = ctx.finish_and_commit(true)?;
            let out_meta = match committed {
                Some((seq, receipt, sum)) => {
                    if let Some(ref s) = sum {
                        self.hub.notify_commit(&committed_schema, s, now_unix_ms());
                    }
                    Some((seq, receipt))
                }
                None => None,
            };
            Ok((value, out_meta))
        })();
        if let Err(ref err) = result
            && err.is_map_full()
        {
            // `OuterGuard` has dropped and released L2 + L3; grow idempotently so caller retry succeeds
            // without silently re-executing the user's closure.
            let _ = eng.gate().grow_map(Some(map_size_before.saturating_mul(2)));
        }
        result
    }

    fn run_combined_hook_cascade(&self, eng: &EngineHandle) -> Result<()> {
        for round in 0..=MAX_HOOK_ROUNDS {
            let round_snap = self.with_active_ctx_mut(|ctx| ctx.take_hook_round())?;
            let Some(snap) = round_snap else {
                return Ok(());
            };
            let matching_host: Vec<HostBeforeCommitFn> = {
                let guard = self.host_hooks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                guard
                    .iter()
                    .filter(|h| h.wants.intersects(&snap.touched_types))
                    .map(|h| Arc::clone(&h.callback))
                    .collect()
            };
            let has_rust = eng.has_matching_hooks(&snap.touched_types);
            if matching_host.is_empty() && !has_rust {
                self.with_active_ctx_mut(|ctx| ctx.finish_hook_round())?;
                return Ok(());
            }
            if round == MAX_HOOK_ROUNDS {
                return Err(ElmdbError::new(
                    ErrorCode::HookCycleLimit,
                    format!("beforeCommit hook cascade exceeded {MAX_HOOK_ROUNDS} rounds"),
                ));
            }
            let sp = self.with_active_ctx_mut(|ctx| ctx.push_savepoint())?;
            let host_delta = if !matching_host.is_empty() {
                Some(self.with_active_ctx(|ctx| build_host_hook_delta(ctx, &snap))?)
            } else {
                None
            };
            if has_rust {
                // Temporarily take `ctx` out of `ACTIVE_TXNS` so NO `RefCell` borrow is held across the
                // Rust `beforeCommit` callback. Re-entering `&Database` from the callback sees the slot
                // present (`has_active_txn() == true`) with `ctx == None`, returning `InvalidArgument`
                // cleanly instead of panicking with `BorrowMutError` or deadlocking on `WriterLock`.
                let taken_ctx = ACTIVE_TXNS.with(|m| -> Result<TxnCtx<'static, LmdbStore, OsVfs, SystemClock>> {
                    let mut map = m.borrow_mut();
                    map.get_mut(&self.id)
                        .and_then(|slot| slot.ctx.take())
                        .ok_or_else(|| ElmdbError::new(ErrorCode::Internal, "active txn ctx missing for rust hook"))
                })?;
                let mut restore = RestoreCtxGuard { db_id: self.id, ctx: Some(taken_ctx) };
                let rust_res = restore
                    .ctx
                    .as_mut()
                    .expect("taken_ctx")
                    .run_rust_hooks_for_round(&snap.touched_types)
                    .and_then(|()| restore.ctx.as_mut().expect("taken_ctx").flush_projections());
                drop(restore);
                if let Err(err) = rust_res {
                    let _ = self.with_active_ctx_mut(|ctx| {
                        ctx.abort_savepoint(sp);
                        Ok(())
                    });
                    return Err(err);
                }
            }
            if let Some(ref delta) = host_delta {
                for cb in matching_host {
                    // No RefCell borrow is held while calling `cb`, so JS hooks can freely call
                    // `get`, `query`, `nextCounter`, `pendingCommitSeq`, `executeWireBatch`, and nested `transact`!
                    match cb(delta) {
                        Ok(Some(wire_bytes)) => {
                            let apply_res = decode_batch(&wire_bytes).and_then(|dec| {
                                self.with_active_ctx_mut(|ctx| ctx.exec_batch_and_flush(&dec.ops).map(|_| ()))
                            });
                            if let Err(err) = apply_res {
                                let _ = self.with_active_ctx_mut(|ctx| {
                                    ctx.abort_savepoint(sp);
                                    Ok(())
                                });
                                return Err(err);
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            let _ = self.with_active_ctx_mut(|ctx| {
                                ctx.abort_savepoint(sp);
                                Ok(())
                            });
                            return Err(err);
                        }
                    }
                }
            }
            self.with_active_ctx_mut(|ctx| {
                ctx.finish_hook_round()?;
                ctx.commit_savepoint(sp)
            })?;
        }
        Ok(())
    }

    /// Executes an atomic batch of `WireOp` mutations, automatically joining an active `transact_scoped`
    /// (with instant RYOW projection flushing) or running as its own atomic transaction.
    pub fn execute_batch(&self, ops: &[WireOp]) -> Result<BatchOutcome> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        if self.has_active_txn() {
            return self.with_active_ctx_mut(|ctx| {
                let created_ids = ctx.exec_batch_and_flush(ops)?;
                Ok(BatchOutcome {
                    commit_seq: ctx.commit_seq(),
                    receipt: elmdb_storage::CommitReceipt { txnid: 0, roots_sha256: [0u8; 32] },
                    created_ids,
                    summary: None,
                })
            });
        }
        // Single choke point: standalone batches also route through `transact_scoped_with_receipt` so host/JS
        // `beforeCommit` hooks enjoy identical RYOW `get`/`query`/`nextCounter`/`pendingCommitSeq` and we return
        // the exact committed `(commit_seq, CommitReceipt)` (never a phantom seq on empty/no-op batches!).
        // Retries automatically strictly on LMDB `MDB_MAP_FULL` by doubling `map_size` under L2 `MapGate::write()`.
        let mut attempts = 0usize;
        let (created_ids, committed) = loop {
            let before = eng.gate().map_size();
            match self.transact_scoped_with_receipt(TransactOptions::default(), || {
                self.with_active_ctx_mut(|ctx| ctx.exec_batch_and_flush(ops))
            }) {
                Ok(res) => break res,
                Err(err) if err.is_map_full() && attempts < 8 => {
                    attempts += 1;
                    let target = before.saturating_mul(2);
                    let after = eng.gate().grow_map(Some(target))?;
                    if after <= before {
                        return Err(err);
                    }
                }
                Err(err) => return Err(err),
            }
        };
        let (commit_seq, receipt) = match committed {
            Some((seq, receipt)) => (seq, receipt),
            None => {
                let (ro, _) = eng.begin_read_pinned()?;
                let seq = read_commit_seq(&ro, &eng.dbs())?;
                let txnid = ro.txnid();
                drop(ro);
                (seq, elmdb_storage::CommitReceipt { txnid, roots_sha256: [0u8; 32] })
            }
        };
        Ok(BatchOutcome { commit_seq, receipt, created_ids, summary: None })
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

    /// Atomically increments generic transactional counter `name` (`1, 2, ...`).
    /// Joins active `transact_scoped` / `beforeCommit` savepoints and rolls back cleanly on abort.
    pub fn next_counter(&self, name: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        if self.has_active_txn() {
            return self.with_active_ctx_mut(|ctx| ctx.next_counter(name));
        }
        eng.next_counter(name)
    }

    /// Reads generic transactional counter `name` (`0` if unset), seeing uncommitted writes inside `transact`.
    pub fn get_counter(&self, name: &str) -> Result<u64> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| ctx.get_counter(name));
        }
        eng.get_counter(name)
    }

    /// Sets generic transactional counter `name` to `value` (rolls back on savepoint/txn abort).
    pub fn set_counter(&self, name: &str, value: u64) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        if self.has_active_txn() {
            return self.with_active_ctx_mut(|ctx| ctx.set_counter(name, value));
        }
        eng.set_counter(name, value)
    }

    /// Runs an atomic read-write closure inside a single LMDB write transaction (Rust `&mut TxnCtx` API).
    /// Routes through the unified `transact_scoped` hook/commit pipeline so both Rust and host/JS
    /// `beforeCommit` hooks run, live watches receive direct delta notifications, and read-only/empty
    /// closures abort cleanly without bumping `commit_seq`.
    pub fn transact<R>(
        &self,
        f: impl FnOnce(&mut TxnCtx<'_, LmdbStore, OsVfs, SystemClock>) -> Result<R>,
    ) -> Result<R> {
        self.forbid_inside_txn("transact")?;
        self.transact_scoped(TransactOptions::default(), || {
            // Temporarily take `ctx` out of `ACTIVE_TXNS` while keeping the slot present (`ctx: None`),
            // so any re-entrant `&Database` call inside `f` returns `InvalidArgument` cleanly instead
            // of panicking with `BorrowMutError` or deadlocking on `WriterLock`.
            let taken_ctx = ACTIVE_TXNS.with(|m| -> Result<TxnCtx<'static, LmdbStore, OsVfs, SystemClock>> {
                let mut map = m.borrow_mut();
                map.get_mut(&self.id)
                    .and_then(|slot| slot.ctx.take())
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Internal, "active txn ctx missing for transact"))
            })?;
            let mut restore = RestoreCtxGuard { db_id: self.id, ctx: Some(taken_ctx) };
            let res = f(restore.ctx.as_mut().expect("taken_ctx"));
            drop(restore);
            res
        })
    }

    pub fn register_before_commit(&self, hook: BeforeCommitHook<LmdbStore, OsVfs, SystemClock>) -> Result<u64> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        Ok(eng.register_before_commit(hook))
    }

    /// Registers a host/JS `beforeCommit` hook that runs with the active transaction parked on the thread,
    /// enabling full RYOW `get`, `query`, `next_counter`, and `pending_commit_seq` inside the callback.
    pub fn register_host_before_commit(&self, wants: TypeMask, callback: HostBeforeCommitFn) -> Result<u64> {
        let _eng = self.enter_op()?;
        self.require_writable()?;
        let id = HOST_HOOK_ID_FLAG | self.next_host_hook_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.host_hooks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(RegisteredHostHook { id, wants, callback });
        Ok(id)
    }

    pub fn unregister_before_commit(&self, id: u64) -> bool {
        if (id & HOST_HOOK_ID_FLAG) != 0 {
            let mut guard = self.host_hooks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = guard.len();
            guard.retain(|h| h.id != id);
            return guard.len() != before;
        }
        let Ok(eng) = self.enter_op() else {
            return false;
        };
        eng.unregister_before_commit(id)
    }

    pub fn get_row(&self, ext_id: &str) -> Result<Option<StoredRow>> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| ctx.get_row(ext_id));
        }
        eng.get_row(ext_id)
    }

    /// Reads one entity row and resolves all `EntityRef` fields inside ONE single pinned snapshot (or active txn).
    pub fn get_row_resolved(&self, ext_id: &str) -> Result<Option<Row>> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| read_row_resolved_in_txn(ctx.txn(), ctx.schema(), &ctx.dbs(), ext_id));
        }
        let (ro, schema) = eng.begin_read_pinned()?;
        read_row_resolved_in_txn(&ro, &schema, &eng.dbs(), ext_id)
    }

    /// Reads edges for `ext_id` inside ONE single pinned snapshot (or active txn).
    pub fn get_edges(&self, ext_id: &str, rel_kind: Option<&str>, dir: EdgeDir) -> Result<Vec<StoredEdge>> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| {
                let dbs = ctx.dbs();
                let mut out = Vec::new();
                if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
                    out.extend(read_relations_out_in_txn(ctx.txn(), ctx.schema(), &dbs, ext_id, rel_kind)?);
                }
                if matches!(dir, EdgeDir::In | EdgeDir::Both) {
                    out.extend(read_relations_in_in_txn(ctx.txn(), ctx.schema(), &dbs, ext_id, rel_kind)?);
                }
                Ok(out)
            });
        }
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

    /// Reads edges for `ext_id` and resolves all endpoint `EntityKey -> ext_id` inside ONE single pinned snapshot (or active txn).
    pub fn get_edges_resolved(&self, ext_id: &str, rel_kind: Option<&str>, dir: EdgeDir) -> Result<Vec<RelationRow>> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| {
                read_edges_resolved_in_txn(ctx.txn(), ctx.schema(), &ctx.dbs(), ext_id, rel_kind, dir)
            });
        }
        let (ro, schema) = eng.begin_read_pinned()?;
        read_edges_resolved_in_txn(&ro, &schema, &eng.dbs(), ext_id, rel_kind, dir)
    }

    /// Sets primary vector embeddings for `ext_id` on `index_name` (O(1) commit into `embeddings` + `vec_pending`),
    /// resolving `ext_id` and its type-scoped vector index atomically inside ONE write txn (zero TOCTOU / ambiguity)
    /// and notifying live vector watches immediately using the exact atomic `MaintReceipt`.
    pub fn set_embeddings(&self, index_name: &str, ext_id: &str, vectors: &[Vec<f32>]) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("setEmbeddings")?;
        let (_idx_id, slot, _hint, mr) = elmdb_vec::set_entity_embeddings_by_ext_id(&eng, index_name, ext_id, vectors)?;
        self.hub.notify_subdb_write_with_maint(mr.seq, mr.schema_epoch, mr.maint_seq, slot, &[0x01], now_unix_ms());
        Ok(())
    }

    /// Executes a typed integer-ID `LogicalPlan` directly (with full RYOW inside an active `transact_scoped`).
    pub fn execute_query(&self, plan: &LogicalPlan, budgets: ExecBudgets<'_>) -> Result<QueryOutput> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| {
                let mut noop = elmdb_query::NoopReadRecorder;
                self.query_engine.execute_in_txn(&eng, ctx.txn(), ctx.schema(), plan, budgets, &mut noop)
            });
        }
        self.query_engine.execute(&eng, plan, budgets)
    }

    /// Parses a string-named `QuerySpec` once at the boundary, executes it inside a single pinned MVCC
    /// snapshot (or active `transact()` for full RYOW), and materializes the external string-ID `QueryAnswer`.
    pub fn execute_query_spec(&self, spec: &QuerySpec, budgets: ExecBudgets<'_>) -> Result<QueryAnswer> {
        let eng = self.enter_op()?;
        if self.has_active_txn() {
            return self.with_active_ctx(|ctx| {
                let dbs = ctx.dbs();
                let schema = ctx.schema();
                let txn = ctx.txn();
                let plan = compile_query_spec(schema, spec, |ext_id| lookup_ext_id_in_txn(txn, &dbs, ext_id))?;
                let mut noop = elmdb_query::NoopReadRecorder;
                let out = self.query_engine.execute_in_txn(&eng, txn, schema, &plan, budgets, &mut noop)?;
                let explain = spec.explain.then_some(&out.explain);
                materialize_query_answer(schema, out.result, explain, |ek| lookup_key_ext_id_in_txn(txn, &dbs, ek))
            });
        }
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
        self.forbid_inside_txn("registerWatch")?;
        let dbs = eng.dbs();
        self.hub.register_query_watch_in_snapshot(
            &eng,
            &self.query_engine,
            ExecBudgets::default(),
            policy,
            now_ms,
            1,
            |ro, schema, exec, unresolved_ext_id| {
                let plan = compile_query_spec(schema, spec, |ext_id| {
                    let found = lookup_ext_id_in_txn(ro, &dbs, ext_id)?;
                    if found.is_none() {
                        *unresolved_ext_id = true;
                    }
                    Ok(found)
                })?;
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
        self.forbid_inside_txn("rerunWatch")?;
        let dbs = eng.dbs();
        self.hub.rerun_query_watch_in_snapshot(
            watch_id,
            &eng,
            &self.query_engine,
            ExecBudgets::default(),
            now_ms,
            cost_ms,
            |ro, schema, exec, unresolved_ext_id| {
                let plan = compile_query_spec(schema, spec, |ext_id| {
                    let found = lookup_ext_id_in_txn(ro, &dbs, ext_id)?;
                    if found.is_none() {
                        *unresolved_ext_id = true;
                    }
                    Ok(found)
                })?;
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
        self.forbid_inside_txn("catchUpCrossProcess")?;
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
    /// notifying `LiveQueryHub` immediately per committed maintenance transaction!
    pub fn drive_background_work(&self, max_entities: usize) -> Result<bool> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("driveBackgroundWork")?;
        let mut more = false;
        let now_ms = now_unix_ms();

        if let Some(mr) = self.registry.drive_build_chunk_with_receipt(&eng, max_entities.max(1))? {
            more = true;
            self.hub.notify_full_rebuild_with_maint(mr.seq, mr.schema_epoch, mr.maint_seq, now_ms);
        }
        let schema = self.fresh_schema(&eng)?;
        for def in &schema.indexes {
            match &def.kind {
                IndexKind::Vector { .. } => {
                    let (dr, receipt) = drain_pending_batch_with_receipt(&eng, def.id, DEFAULT_DRAIN_BATCH)?;
                    if let Some((slot, mr)) = receipt {
                        self.hub.notify_subdb_write_with_maint(
                            mr.seq,
                            mr.schema_epoch,
                            mr.maint_seq,
                            slot,
                            &[0x01],
                            now_ms,
                        );
                    }
                    if dr.processed > 0 || dr.needs_compaction {
                        more = true;
                        if dr.needs_compaction {
                            let (_, cmr) = elmdb_vec::compact_now_with_receipt(&eng, def.id)?;
                            self.hub.notify_schema_change_with_maint(cmr.seq, cmr.schema_epoch, cmr.maint_seq, now_ms);
                        }
                    }
                }
                IndexKind::Rank { .. } => {
                    let (pushes, receipt) = rank::drain_rank_chunk_with_receipt(&eng, def.id, 1024)?;
                    if let Some((slot, mr)) = receipt {
                        self.hub.notify_subdb_write_with_maint(
                            mr.seq,
                            mr.schema_epoch,
                            mr.maint_seq,
                            slot,
                            &[0x01],
                            now_ms,
                        );
                    }
                    if pushes > 0 {
                        more = true;
                    }
                }
                _ => {}
            }
        }
        Ok(more)
    }

    /// Completes all pending deferred index builds, HNSW pending queues, PageRank push queues, notifies watches
    /// immediately per committed maintenance chunk, and fsyncs cleanly.
    pub fn flush(&self) -> Result<()> {
        let eng = self.enter_op()?;
        self.forbid_inside_txn("flush")?;
        self.require_writable()?;
        let now_ms = now_unix_ms();
        while let Some(mr) = self.registry.drive_build_chunk_with_receipt(&eng, 2048)? {
            self.hub.notify_full_rebuild_with_maint(mr.seq, mr.schema_epoch, mr.maint_seq, now_ms);
        }
        let schema = self.fresh_schema(&eng)?;
        for def in &schema.indexes {
            match &def.kind {
                IndexKind::Vector { .. } => {
                    let needs_compact = loop {
                        let (dr, receipt) = drain_pending_batch_with_receipt(&eng, def.id, DEFAULT_DRAIN_BATCH)?;
                        if let Some((slot, mr)) = receipt {
                            self.hub.notify_subdb_write_with_maint(
                                mr.seq,
                                mr.schema_epoch,
                                mr.maint_seq,
                                slot,
                                &[0x01],
                                now_ms,
                            );
                        }
                        if dr.processed == 0 {
                            break dr.needs_compaction;
                        }
                    };
                    if needs_compact {
                        let (_, cmr) = elmdb_vec::compact_now_with_receipt(&eng, def.id)?;
                        self.hub.notify_schema_change_with_maint(cmr.seq, cmr.schema_epoch, cmr.maint_seq, now_ms);
                    }
                }
                IndexKind::Rank { .. } => loop {
                    let (pushes, receipt) =
                        rank::drain_rank_chunk_with_receipt(&eng, def.id, rank::DEFAULT_PUSH_BUDGET)?;
                    if let Some((slot, mr)) = receipt {
                        self.hub.notify_subdb_write_with_maint(
                            mr.seq,
                            mr.schema_epoch,
                            mr.maint_seq,
                            slot,
                            &[0x01],
                            now_ms,
                        );
                    }
                    if pushes == 0 {
                        break;
                    }
                },
                _ => {}
            }
        }
        eng.sync()?;
        Ok(())
    }

    pub fn index_status(&self) -> Result<Vec<IndexStatus>> {
        let eng = self.enter_op()?;
        self.forbid_inside_txn("indexStatus")?;
        let schema = self.fresh_schema(&eng)?;
        let raw = self.registry.index_status(&eng)?;
        Ok(raw.iter().map(|st| IndexStatus::from_proj(&schema, st)).collect())
    }

    pub fn check(&self, index_name: Option<&str>) -> Result<CheckReport> {
        let eng = self.enter_op()?;
        self.forbid_inside_txn("check")?;
        let schema = self.fresh_schema(&eng)?;
        let idx_id = match index_name {
            Some(name) => Some(resolve_unique_index_id(&schema, name)?),
            None => None,
        };
        let report = self.registry.check(&eng, idx_id)?;
        Ok(CheckReport::from_proj(report))
    }

    pub fn rebuild(&self, index_name: Option<&str>) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("rebuild")?;
        let schema = self.fresh_schema(&eng)?;
        let idx_id = match index_name {
            Some(name) => Some(resolve_unique_index_id(&schema, name)?),
            None => None,
        };
        let (_, mr) = self.registry.rebuild_with_receipt(&eng, idx_id)?;
        self.query_engine.clear_cache();
        self.hub.notify_full_rebuild_with_maint(mr.seq, mr.schema_epoch, mr.maint_seq, now_unix_ms());
        Ok(())
    }

    /// Destroys all entities belonging to an archived type in a single atomic transaction.
    /// Routes through `transact_scoped` so both Rust and host/JS `beforeCommit` hooks run,
    /// live watches receive direct deltas, and 0-row purges abort without bumping `commit_seq`.
    pub fn purge_archived(&self, type_name: &str) -> Result<u64> {
        self.forbid_inside_txn("purgeArchived")?;
        self.transact_scoped(TransactOptions::default(), || {
            self.with_active_ctx_mut(|tx| {
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
            })
        })
    }

    pub fn stats(&self) -> Result<DatabaseStats> {
        let eng = self.enter_op()?;
        self.forbid_inside_txn("stats")?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();
        let commit_seq = read_commit_seq(&ro, &dbs)?;
        let txnid = ro.txnid();
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
        self.forbid_inside_txn("backup")?;
        let target = target.as_ref();
        crate::fs_publish::ensure_not_self_or_descendant(&self.dir, target)?;

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

        let parent = crate::fs_publish::parent_dir(target);
        fs::create_dir_all(parent)
            .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot create backup parent: {e}")))?;

        let nonce = NEXT_STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(".elmdb-backup-tmp-{}-{nonce}", std::process::id()));
        let mut cleanup = DirCleanupGuard(Some(staging.clone()));

        eng.gate().copy_compacted(&staging)?;
        {
            use std::io::Write;
            let mut marker = fs::File::create(staging.join(BACKUP_MARKER))
                .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot create backup marker: {e}")))?;
            marker
                .write_all(b"elmdb-v2-backup\n")
                .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot write backup marker: {e}")))?;
            marker.sync_all().map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot sync backup marker: {e}")))?;
        }

        // Re-validate self-overwrite right before publishing so a concurrent symlink/mount race cannot bypass it.
        crate::fs_publish::ensure_not_self_or_descendant(&self.dir, target)?;
        match into {
            BackupInto::New => crate::fs_publish::atomic_publish_dir_new(&staging, target)?,
            BackupInto::Backups => {
                crate::fs_publish::atomic_publish_dir_replace(&staging, target, BACKUP_MARKER, nonce)?;
            }
        }
        cleanup.disarm();
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
        let in_self_txn = self.has_active_txn();
        if propagate_sync_err && in_self_txn {
            self.forbid_inside_txn("close")?;
        }
        if !propagate_sync_err && in_self_txn {
            // Process exit while a transact() is still on the calling thread's stack:
            // abort its thread-local RwTxn immediately and skip waiting on our own OpGuard.
            let _ = ACTIVE_TXNS.try_with(|m| {
                if let Ok(mut map) = m.try_borrow_mut() {
                    let _ = map.remove(&self.id);
                }
            });
        }
        if self.closed.swap(true, Ordering::SeqCst) {
            let mut st = self.close_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let deadline = timeout.map(|d| std::time::Instant::now() + d);
            while !st.done {
                match deadline {
                    Some(dl) => {
                        let now = std::time::Instant::now();
                        if now >= dl {
                            return if !propagate_sync_err {
                                Ok(())
                            } else if self.read_only {
                                Err(ElmdbError::new(
                                    ErrorCode::DatabaseLocked,
                                    "database close timed out waiting for concurrent close",
                                ))
                            } else {
                                Err(ElmdbError::new(
                                    ErrorCode::DurabilityFailed,
                                    "database close timed out waiting for concurrent close",
                                ))
                            };
                        }
                        let (next_st, _) = self
                            .close_cv
                            .wait_timeout(st, dl.saturating_duration_since(now))
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        st = next_st;
                    }
                    None => {
                        st = self.close_cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
            }
            if propagate_sync_err && let Some((code, msg)) = &st.outcome {
                return Err(ElmdbError::new(*code, msg.clone()));
            }
            return Ok(());
        }

        struct MarkCloseDone<'a> {
            db: &'a Database,
            outcome: Option<(ErrorCode, String)>,
        }
        impl Drop for MarkCloseDone<'_> {
            fn drop(&mut self) {
                let mut st = self.db.close_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                st.outcome = self.outcome.take();
                st.done = true;
                self.db.close_cv.notify_all();
            }
        }
        // Default to a failure outcome so unwinding on panic never reports `Ok(())` to waiters.
        let mut mark_done = MarkCloseDone {
            db: self,
            outcome: Some((ErrorCode::DurabilityFailed, "database close panicked before completing".to_owned())),
        };

        self.hub.stop_watches();

        // Take the engine handle out under `engine.write()`, which acts as a full barrier against
        // any concurrent `enter_op` acquiring `engine.read()` + incrementing `active_ops`.
        let taken = self.engine.write().unwrap_or_else(std::sync::PoisonError::into_inner).take();

        // Wait for already-entered operations to drain cleanly via `drain_cv` (unless this very
        // thread is tearing down inside its own aborted active transaction).
        let drained = if in_self_txn {
            false
        } else {
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

        let outcome = if !drained {
            drop(taken);
            if self.read_only {
                Err(ElmdbError::new(
                    ErrorCode::DatabaseLocked,
                    "database close timed out waiting for active operations to drain",
                ))
            } else {
                Err(ElmdbError::new(
                    ErrorCode::DurabilityFailed,
                    "database close timed out waiting for active operations to drain; durability sync skipped",
                ))
            }
        } else if let Some(eng) = taken {
            let sync_res = if self.read_only { Ok(()) } else { eng.sync() };
            drop(eng);
            sync_res
        } else {
            Ok(())
        };

        mark_done.outcome = outcome.as_ref().err().map(|err| (err.code, err.message.clone()));
        drop(mark_done);
        if propagate_sync_err { outcome } else { Ok(()) }
    }

    /// Restores persisted entity and relation timestamps during lossless `dump::load`.
    pub(crate) fn restore_loaded_timestamps(
        &self,
        entity_timestamps: &[(String, i64, i64, i64)],
        edge_timestamps: &[(String, String, String, i64)],
    ) -> Result<()> {
        let eng = self.enter_op()?;
        self.require_writable()?;
        self.forbid_inside_txn("restore_loaded_timestamps")?;
        eng.restore_loaded_timestamps(entity_timestamps, edge_timestamps)
    }

    /// Single-snapshot scan of schema, raw primary rows (preserving `EntityRef` vs `DanglingRef`),
    /// outgoing edges, embeddings, and named counters (used by `dump`).
    pub(crate) fn snapshot_for_dump(&self) -> Result<DumpSnapshot> {
        let eng = self.enter_op()?;
        self.forbid_inside_txn("dump")?;
        let (ro, schema) = eng.begin_read_pinned()?;
        let dbs = eng.dbs();

        let mut rows = Vec::new();
        {
            let mut cur = ro.cursor(dbs.entities)?;
            let mut item = cur.first()?;
            while let Some((k, _)) = item {
                if k.len() == 8 {
                    let ek = EntityKey::from_ne_bytes(k.try_into().expect("8"));
                    if let Some(row) = elmdb_engine::read_raw_stored_row(&ro, &schema, &dbs, ek)? {
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
        let counters = list_counters_in_txn(&ro, &dbs)?;
        Ok(DumpSnapshot { schema, rows, edges, embeddings, counters })
    }
}

pub(crate) struct DumpSnapshot {
    pub schema: Arc<CompiledSchema>,
    pub rows: Vec<StoredRow>,
    pub edges: Vec<StoredEdge>,
    pub embeddings: Vec<(String, String, Vec<Vec<f32>>)>,
    pub counters: BTreeMap<String, u64>,
}

fn read_row_resolved_in_txn<R: KvRead>(
    ro: &R,
    schema: &CompiledSchema,
    dbs: &elmdb_engine::SystemDbs,
    ext_id: &str,
) -> Result<Option<Row>> {
    let Some(ek) = lookup_ext_id_in_txn(ro, dbs, ext_id)? else {
        return Ok(None);
    };
    let Some(stored) = read_stored_row(ro, schema, dbs, ek)? else {
        return Ok(None);
    };
    let mut cache: HashMap<EntityKey, String> = HashMap::new();
    let mut resolve_key = |k: EntityKey| -> Result<String> {
        if let Some(cached) = cache.get(&k) {
            return Ok(cached.clone());
        }
        let resolved = lookup_key_ext_id_in_txn(ro, dbs, k)?.unwrap_or_else(|| format!("__deleted:{}", k.0));
        cache.insert(k, resolved.clone());
        Ok(resolved)
    };
    Ok(Some(Row::from_stored_resolved(&stored, &mut resolve_key)?))
}

fn read_edges_resolved_in_txn<R: KvRead>(
    ro: &R,
    schema: &CompiledSchema,
    dbs: &elmdb_engine::SystemDbs,
    ext_id: &str,
    rel_kind: Option<&str>,
    dir: EdgeDir,
) -> Result<Vec<RelationRow>> {
    let mut raw = Vec::new();
    if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
        raw.extend(read_relations_out_in_txn(ro, schema, dbs, ext_id, rel_kind)?);
    }
    if matches!(dir, EdgeDir::In | EdgeDir::Both) {
        raw.extend(read_relations_in_in_txn(ro, schema, dbs, ext_id, rel_kind)?);
    }
    let mut cache: HashMap<EntityKey, String> = HashMap::new();
    let mut out = Vec::with_capacity(raw.len());
    for e in &raw {
        let src_id = if let Some(s) = cache.get(&e.src) {
            s.clone()
        } else {
            let s = lookup_key_ext_id_in_txn(ro, dbs, e.src)?.unwrap_or_else(|| format!("__deleted:{}", e.src.0));
            cache.insert(e.src, s.clone());
            s
        };
        let dst_id = if let Some(s) = cache.get(&e.dst) {
            s.clone()
        } else {
            let s = lookup_key_ext_id_in_txn(ro, dbs, e.dst)?.unwrap_or_else(|| format!("__deleted:{}", e.dst.0));
            cache.insert(e.dst, s.clone());
            s
        };
        out.push(RelationRow::from_stored(e, src_id, dst_id));
    }
    Ok(out)
}

fn build_host_hook_delta(
    ctx: &TxnCtx<'static, LmdbStore, OsVfs, SystemClock>,
    snap: &HookRoundSnapshot,
) -> Result<HostHookDelta> {
    let sc = ctx.schema();
    let entities =
        snap.entity_deltas
            .iter()
            .filter_map(|d| {
                if d.before.is_none() && d.after.is_none() {
                    return None;
                }
                let type_name = sc.type_by_id(d.type_id).map(|t| t.name.clone()).unwrap_or_default();
                let id = d.after.as_ref().or(d.before.as_ref()).map(|r| r.header.ext_id.clone()).unwrap_or_default();
                let op =
                    match (&d.before, &d.after) {
                        (None, Some(_)) => "create",
                        (Some(_), None) => "destroy",
                        (Some(b), Some(a)) if b.header.is_trashed() != a.header.is_trashed() => {
                            if a.header.is_trashed() { "trash" } else { "restore" }
                        }
                        _ => "update",
                    };
                Some(HostEntityDelta { id, type_name, op })
            })
            .collect();
    let mut edges = Vec::with_capacity(snap.edge_deltas.len());
    for e in &snap.edge_deltas {
        if e.before.is_none() && e.after.is_none() {
            continue;
        }
        let rel_kind = sc.relation_by_id(e.kind).map(|r| r.name.clone()).unwrap_or_default();
        let op = match (&e.before, &e.after) {
            (None, Some(_)) => "link",
            (Some(_), None) => "unlink",
            _ => "update",
        };
        let endpoints = e.after.as_ref().or(e.before.as_ref()).map(|pe| (pe.src, pe.dst));
        let (source_id, target_id) = match endpoints {
            Some((src_key, dst_key)) => (
                ctx.resolve_ext_id(src_key)?.unwrap_or_else(|| format!("__deleted:{}", src_key.0)),
                ctx.resolve_ext_id(dst_key)?.unwrap_or_else(|| format!("__deleted:{}", dst_key.0)),
            ),
            None => (String::new(), String::new()),
        };
        edges.push(HostEdgeDelta { rel_kind, op, source_id, target_id });
    }
    Ok(HostHookDelta { commit_seq: snap.commit_seq, entities, edges })
}

struct RestoreCtxGuard {
    db_id: u64,
    ctx: Option<TxnCtx<'static, LmdbStore, OsVfs, SystemClock>>,
}

impl Drop for RestoreCtxGuard {
    fn drop(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            let _ = ACTIVE_TXNS.try_with(|m| {
                if let Ok(mut map) = m.try_borrow_mut()
                    && let Some(slot) = map.get_mut(&self.db_id)
                {
                    slot.ctx = Some(ctx);
                }
            });
        }
    }
}

fn resolve_unique_index_id(schema: &CompiledSchema, name: &str) -> Result<IndexId> {
    // Check exact unqualified matches first so dotted index names (e.g. `v1.emb`) resolve cleanly.
    let exact: Vec<_> = schema.indexes.iter().filter(|i| i.name == name).collect();
    if exact.len() == 1 {
        return Ok(exact[0].id);
    }
    if exact.len() > 1 {
        let qualified: Vec<String> = exact
            .iter()
            .map(|i| {
                let tn = schema.type_by_id(i.type_id).map_or("?", |t| t.name.as_str());
                format!("{tn}.{}", i.name)
            })
            .collect();
        return Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!(
                "ambiguous index name '{name}' matches multiple types ({}); qualify as 'TypeName.{name}'",
                qualified.join(", ")
            ),
        ));
    }
    if let Some((type_name, idx_name)) = name.split_once('.') {
        let tdef = schema
            .type_by_name_any(type_name)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{type_name}'")))?;
        return schema
            .indexes
            .iter()
            .find(|i| i.type_id == tdef.id && i.name == idx_name)
            .map(|i| i.id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{name}'")));
    }
    Err(ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{name}'")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_mode_clears_mdb_nosync_while_grouped_and_unsafe_set_it() {
        for (mode, expect_no_sync) in
            [(DurabilityMode::Strict, false), (DurabilityMode::Grouped, true), (DurabilityMode::Unsafe, true)]
        {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path(), OpenOptions { durability: mode, ..Default::default() }).unwrap();
            let is_no_sync = db.engine.read().unwrap().as_ref().unwrap().gate().store().is_no_sync();
            assert_eq!(is_no_sync, expect_no_sync, "unexpected MDB_NOSYNC state for {mode:?}");
            db.close().unwrap();
        }
    }

    #[test]
    fn batch_b_close_within_timeout_reports_error_for_writable_readonly_and_concurrent_waiters() {
        // 1. Writable DB: active op blocks drain -> `close_within` returns DurabilityFailed,
        //    concurrent waiter with shorter timeout times out cleanly, concurrent waiter without
        //    shorter timeout sees recorded DurabilityFailed, and `close_at_exit` returns Ok(()).
        {
            let dir = tempfile::tempdir().unwrap();
            let db = Arc::new(Database::open(dir.path(), OpenOptions::default()).unwrap());
            let held_op = db.enter_op().unwrap();

            let db_closer = Arc::clone(&db);
            let closer = std::thread::spawn(move || db_closer.close_within(Some(Duration::from_millis(500))));
            while !db.closed.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            // Waiter with shorter timeout fails immediately on waiter deadline (writable -> DurabilityFailed).
            let waiter_err = db.close_within(Some(Duration::from_millis(15))).unwrap_err();
            assert_eq!(waiter_err.code, ErrorCode::DurabilityFailed);
            assert!(waiter_err.message.contains("concurrent close"), "got: {}", waiter_err.message);
            // `close_at_exit` waiter timing out returns Ok(()).
            db.close_at_exit(Duration::from_millis(15)).unwrap();
            // Unbounded waiter waits for closer to finish and sees the recorded DurabilityFailed.
            let db_waiter = Arc::clone(&db);
            let waiter = std::thread::spawn(move || db_waiter.close());

            let err1 = closer.join().unwrap().unwrap_err();
            assert_eq!(err1.code, ErrorCode::DurabilityFailed);
            let err2 = waiter.join().unwrap().unwrap_err();
            assert_eq!(err2.code, ErrorCode::DurabilityFailed);
            db.close_at_exit(Duration::from_millis(10)).unwrap();
            drop(held_op);
        }

        // 1b. Concurrent waiter with generous timeout waits for a succeeding closer and returns Ok(()).
        {
            let dir = tempfile::tempdir().unwrap();
            let db = Arc::new(Database::open(dir.path(), OpenOptions::default()).unwrap());
            let held_op = db.enter_op().unwrap();

            let db_closer = Arc::clone(&db);
            let closer = std::thread::spawn(move || db_closer.close_within(Some(Duration::from_millis(500))));
            while !db.closed.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let db_waiter = Arc::clone(&db);
            let waiter = std::thread::spawn(move || db_waiter.close_within(Some(Duration::from_millis(500))));
            std::thread::sleep(Duration::from_millis(25));
            drop(held_op);

            closer.join().unwrap().unwrap();
            waiter.join().unwrap().unwrap();
        }

        // 2. First closer via `close_at_exit` on active-op drain timeout returns Ok(()) (does not propagate).
        {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path(), OpenOptions::default()).unwrap();
            let held_op = db.enter_op().unwrap();
            db.close_at_exit(Duration::from_millis(15)).unwrap();
            assert_eq!(db.close().unwrap_err().code, ErrorCode::DurabilityFailed);
            drop(held_op);
        }

        // 3. Read-only DB: active op blocks drain -> `close_within` and concurrent waiter return DatabaseLocked.
        {
            let dir = tempfile::tempdir().unwrap();
            let rw = Database::open(dir.path(), OpenOptions::default()).unwrap();
            rw.close().unwrap();

            let ro =
                Arc::new(Database::open(dir.path(), OpenOptions { read_only: true, ..Default::default() }).unwrap());
            let held_op = ro.enter_op().unwrap();
            let ro_closer = Arc::clone(&ro);
            let closer = std::thread::spawn(move || ro_closer.close_within(Some(Duration::from_millis(500))));
            while !ro.closed.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let ro_waiter_err = ro.close_within(Some(Duration::from_millis(15))).unwrap_err();
            assert_eq!(ro_waiter_err.code, ErrorCode::DatabaseLocked);
            assert!(ro_waiter_err.message.contains("concurrent close"), "got: {}", ro_waiter_err.message);
            let err = closer.join().unwrap().unwrap_err();
            assert_eq!(err.code, ErrorCode::DatabaseLocked);
            assert_eq!(ro.close().unwrap_err().code, ErrorCode::DatabaseLocked);
            drop(held_op);
        }

        // 4. Clean drain on writable DB succeeds and second `close()` is idempotent Ok(()).
        {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path(), OpenOptions::default()).unwrap();
            db.close().unwrap();
            db.close().unwrap();
        }
    }

    #[test]
    fn batch_b_fs_publish_guards_self_descendant_and_atomic_file_no_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_db");
        let sibling = dir.path().join("sibling_backup");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&sibling).unwrap();

        // Self, descendant, lexical `..` into self, and ancestor rejected; sibling on same filesystem allowed.
        assert_eq!(
            crate::fs_publish::ensure_not_self_or_descendant(&src, &src).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            crate::fs_publish::ensure_not_self_or_descendant(&src, &src.join("nested/sub")).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            crate::fs_publish::ensure_not_self_or_descendant(&src, &src.join("missing/../sub")).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        // `..` starting OUTSIDE `src` and resolving INTO `src` (kills `normalize_absolute`/`ParentDir` mutants on Linux).
        assert_eq!(
            crate::fs_publish::ensure_not_self_or_descendant(&src, &dir.path().join("missing/../src_db"))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            crate::fs_publish::ensure_not_self_or_descendant(&src, &dir.path().join("missing/../src_db/child"))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            crate::fs_publish::ensure_not_self_or_descendant(&src, dir.path()).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        crate::fs_publish::ensure_not_self_or_descendant(&src, &sibling).unwrap();
        crate::fs_publish::ensure_not_self_or_descendant(&src, &dir.path().join("nonexistent_sibling")).unwrap();

        // `atomic_publish_file_no_overwrite` publishes into new path and refuses to overwrite existing file.
        let tmp1 = dir.path().join(".tmp1");
        let target = dir.path().join("out.json");
        fs::write(&tmp1, b"first").unwrap();
        crate::fs_publish::atomic_publish_file_no_overwrite(&tmp1, &target).unwrap();
        assert!(!tmp1.exists());
        assert_eq!(fs::read(&target).unwrap(), b"first");

        let tmp2 = dir.path().join(".tmp2");
        fs::write(&tmp2, b"second").unwrap();
        let err = crate::fs_publish::atomic_publish_file_no_overwrite(&tmp2, &target).unwrap_err();
        assert_eq!(err.code, ErrorCode::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"first", "existing target file must remain intact");
        let _ = fs::remove_file(&tmp2);

        // Non-AlreadyExists errors map to Io (missing source file / missing target parent / regular file on Windows).
        assert_eq!(
            crate::fs_publish::atomic_publish_file_no_overwrite(&dir.path().join("no_such_tmp"), &dir.path().join("x"))
                .unwrap_err()
                .code,
            ErrorCode::Io
        );
        let stage_io = dir.path().join("stage_io");
        fs::create_dir_all(&stage_io).unwrap();
        assert_eq!(
            crate::fs_publish::atomic_publish_dir_new(&stage_io, &dir.path().join("no_such_parent/child"))
                .unwrap_err()
                .code,
            ErrorCode::Io
        );
        let reg_file = dir.path().join("regular_file.txt");
        fs::write(&reg_file, b"file").unwrap();
        let reg_err = crate::fs_publish::atomic_publish_dir_new(&stage_io, &reg_file).unwrap_err();
        #[cfg(unix)]
        assert_eq!(reg_err.code, ErrorCode::AlreadyExists);
        #[cfg(not(unix))]
        assert_eq!(reg_err.code, ErrorCode::Io);

        // `atomic_publish_dir_new` directly rejects non-empty target dir with AlreadyExists.
        let non_empty = dir.path().join("non_empty_dir");
        fs::create_dir_all(&non_empty).unwrap();
        fs::write(non_empty.join("f.txt"), b"x").unwrap();
        let stage_ne = dir.path().join("stage_ne");
        fs::create_dir_all(&stage_ne).unwrap();
        assert_eq!(
            crate::fs_publish::atomic_publish_dir_new(&stage_ne, &non_empty).unwrap_err().code,
            ErrorCode::AlreadyExists
        );

        // `atomic_publish_dir_replace` publishes into non-existent target, existing empty target,
        // rejects non-backup non-empty target, and rejects symlink target.
        let fresh_target = dir.path().join("fresh_replace_target");
        crate::fs_publish::atomic_publish_dir_replace(&stage_ne, &fresh_target, BACKUP_MARKER, 1).unwrap();
        assert!(fresh_target.exists());

        let empty_replace_target = dir.path().join("empty_replace_target");
        fs::create_dir_all(&empty_replace_target).unwrap();
        let stage_empty = dir.path().join("stage_empty");
        fs::create_dir_all(&stage_empty).unwrap();
        fs::write(stage_empty.join("ok.txt"), b"ok").unwrap();
        crate::fs_publish::atomic_publish_dir_replace(&stage_empty, &empty_replace_target, BACKUP_MARKER, 2).unwrap();
        assert_eq!(fs::read(empty_replace_target.join("ok.txt")).unwrap(), b"ok");

        assert_eq!(
            crate::fs_publish::atomic_publish_dir_replace(&stage_io, &non_empty, BACKUP_MARKER, 3).unwrap_err().code,
            ErrorCode::AlreadyExists
        );

        #[cfg(unix)]
        {
            let link_target = dir.path().join("symlink_backup");
            std::os::unix::fs::symlink(&fresh_target, &link_target).unwrap();
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_replace(&stage_io, &link_target, BACKUP_MARKER, 4)
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidArgument
            );
            // Item 1.6 + P2-1: `atomic_publish_dir_new` & `atomic_publish_dir_replace` ALSO reject symlink
            // targets WITH OR WITHOUT trailing path separators without removing or following the symlink!
            let link_with_slash = PathBuf::from(format!("{}/", link_target.display()));
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_new(&stage_io, &link_target).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_new(&stage_io, &link_with_slash).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_replace(&stage_io, &link_with_slash, BACKUP_MARKER, 5)
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidArgument
            );
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_new(&stage_io, &dir.path().join(".")).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_new(&stage_io, Path::new("")).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_new(&stage_io, Path::new(".")).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            // Non-NotFound error (`ENOTDIR` traversing through a regular file) maps to `ErrorCode::Io`!
            assert_eq!(
                crate::fs_publish::atomic_publish_dir_new(&stage_io, &reg_file.join("child")).unwrap_err().code,
                ErrorCode::Io
            );
            assert_eq!(
                crate::fs_publish::ensure_not_self_or_descendant(&src, &reg_file.join("child")).unwrap_err().code,
                ErrorCode::Io
            );
            assert!(
                fs::symlink_metadata(&link_target).unwrap().file_type().is_symlink(),
                "symlink target must remain intact after failed atomic_publish_dir_new"
            );

            // Item 1.5: Symlink into `src/sub` + `../nonexistent_leaf` MUST be caught BEFORE lexical `..`!
            let src_sub = src.join("sub");
            fs::create_dir_all(&src_sub).unwrap();
            let sym_into_src = dir.path().join("sym_to_src_sub");
            std::os::unix::fs::symlink(&src_sub, &sym_into_src).unwrap();
            assert_eq!(
                crate::fs_publish::ensure_not_self_or_descendant(&src, &sym_into_src.join("../new_leaf"))
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidArgument
            );
            assert_eq!(
                crate::fs_publish::ensure_not_self_or_descendant(&src, &sym_into_src.join("..")).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            // Dangling symlink pointing into `src` must also be rejected fail-closed!
            let dangling = dir.path().join("dangling_into_src");
            std::os::unix::fs::symlink(src.join("nonexistent_child"), &dangling).unwrap();
            assert_eq!(
                crate::fs_publish::ensure_not_self_or_descendant(&src, &dangling).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            // Symlink to a valid sibling + `../other_nonexistent` is ALLOWED.
            let sib_sub = sibling.join("nested");
            fs::create_dir_all(&sib_sub).unwrap();
            let sym_sib = dir.path().join("sym_sib");
            std::os::unix::fs::symlink(&sib_sub, &sym_sib).unwrap();
            crate::fs_publish::ensure_not_self_or_descendant(&src, &sym_sib.join("../ok_leaf")).unwrap();
        }

        // `parent_dir` returns `.` for bare filenames and real parent for nested paths.
        assert_eq!(crate::fs_publish::parent_dir(Path::new("dump.json")), Path::new("."));
        assert_eq!(crate::fs_publish::parent_dir(&target), dir.path());
    }

    #[test]
    fn pr268_batch1_transact_map_full_grows_for_retry_and_cross_db_nested_locks_and_backup_unacked() {
        use std::collections::BTreeMap;

        use elmdb_wire::{CanonicalValue, WireOp};

        use crate::protocol::{FieldDefinition, FieldSpec, Schema, TypeDefinition};

        // 1. Item 1.4: `transact_scoped()` hitting `MDB_MAP_FULL` grows the map after aborting (without re-running
        //    the closure) so an immediate caller retry succeeds cleanly!
        let dir1 = tempfile::tempdir().unwrap();
        let db1 = Database::open(
            dir1.path(),
            OpenOptions { map_size: Some(128 * 1024), durability: DurabilityMode::Grouped, ..OpenOptions::default() },
        )
        .unwrap();
        let mut fields_def = BTreeMap::new();
        fields_def.insert("body".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
        let mut types = BTreeMap::new();
        types.insert(
            "doc".into(),
            TypeDefinition {
                fields: fields_def,
                indexes: BTreeMap::new(),
                additional_fields: false,
                short_code: None,
                change_order: false,
                declared_by: None,
            },
        );
        let schema = Schema { dollar_schema: None, types, relations: BTreeMap::new() };
        db1.set_schema(&schema).unwrap();

        let initial_map = db1.stats().unwrap().map_size_bytes;
        // Generate genuinely incompressible ASCII payloads (8 * 32 KiB = 256 KiB) so zstd blob compression
        // cannot shrink them and the first transact_scoped() deterministically hits MDB_MAP_FULL on 4/16/64 KiB pages.
        let payloads: Vec<String> = (0..8u64)
            .map(|i| {
                let mut s = String::with_capacity(32 * 1024);
                let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (i.wrapping_add(1).wrapping_mul(0xBF58_476D_1CE4_E5B9));
                for _ in 0..(32 * 1024) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    s.push((b'!' + (x % 93) as u8) as char);
                }
                s
            })
            .collect();
        let mut calls = 0usize;
        let first_err = db1
            .transact_scoped(TransactOptions::default(), || {
                calls += 1;
                for (i, payload) in payloads.iter().enumerate() {
                    db1.execute_batch(&[WireOp::Create {
                        type_name: "doc".into(),
                        external_id: Some(format!("doc_{i}")),
                        fields: vec![("body".into(), CanonicalValue::String(payload.clone()))],
                    }])?;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(first_err.is_map_full(), "expected MapFull/StorageFull, got {first_err:?}");
        assert_eq!(calls, 1, "transact_scoped() must NOT silently re-execute user closure on MapFull");
        assert!(
            db1.stats().unwrap().map_size_bytes > initial_map,
            "transact_scoped() must grow map_size after outermost MapFull abort"
        );

        // Retry (growing further if needed on larger OS pages) now makes forward progress and succeeds!
        let mut retries = 0usize;
        loop {
            match db1.transact_scoped(TransactOptions::default(), || {
                for (i, payload) in payloads.iter().enumerate() {
                    db1.execute_batch(&[WireOp::Create {
                        type_name: "doc".into(),
                        external_id: Some(format!("doc_{i}")),
                        fields: vec![("body".into(), CanonicalValue::String(payload.clone()))],
                    }])?;
                }
                Ok(())
            }) {
                Ok(()) => break,
                Err(err) if err.is_map_full() && retries < 3 => {
                    retries += 1;
                    continue;
                }
                Err(err) => panic!("unexpected transact retry failure: {err:?}"),
            }
        }
        assert!(db1.get_row("doc_7").unwrap().is_some());

        // 2. Item 1.8: Cross-DB nested read inside `db1.transact_scoped()` works in debug builds without lock-tracker panic!
        let dir2 = tempfile::tempdir().unwrap();
        let db2 = Database::open(dir2.path(), OpenOptions::default()).unwrap();
        db2.set_schema(&schema).unwrap();
        db2.execute_batch(&[WireOp::Create {
            type_name: "doc".into(),
            external_id: Some("other".into()),
            fields: vec![("body".into(), CanonicalValue::String("from_db2".into()))],
        }])
        .unwrap();

        let read_body = db1
            .transact_scoped(TransactOptions::default(), || {
                let fetched = db2.get_row("other")?.expect("db2 row visible inside db1.transact_scoped()");
                let text = match fetched.fields.get("body") {
                    Some(CanonicalValue::String(s)) => s.clone(),
                    other => panic!("unexpected value: {other:?}"),
                };
                db1.execute_batch(&[WireOp::Create {
                    type_name: "doc".into(),
                    external_id: Some("copied".into()),
                    fields: vec![("body".into(), CanonicalValue::String(text.clone()))],
                }])?;
                Ok(text)
            })
            .unwrap();
        assert_eq!(read_body, "from_db2");
        assert!(db1.get_row("copied").unwrap().is_some());

        // 3. Item 1.3: `Database::backup_into` refuses to seal a Clean backup over an `UnsyncedAfterReboot` DB!
        let dir3 = tempfile::tempdir().unwrap();
        {
            let db3 = Database::open(
                dir3.path(),
                OpenOptions { durability: DurabilityMode::Grouped, ..OpenOptions::default() },
            )
            .unwrap();
            db3.set_schema(&schema).unwrap();
            db3.flush().unwrap();
            db3.close().unwrap();
        }
        // Simulate unacknowledged reboot by removing sidecar while DB has committed txns.
        fs::remove_file(dir3.path().join(elmdb_storage::SYNC_SIDECAR_NAME)).unwrap();
        let db3 =
            Database::open(dir3.path(), OpenOptions { durability: DurabilityMode::Grouped, ..OpenOptions::default() })
                .unwrap();
        assert_eq!(db3.open_report().status, "unsyncedAfterReboot");
        let bak_dir = tempfile::tempdir().unwrap();
        let bak_target = bak_dir.path().join("bak");
        let bak_err = db3.backup_into(&bak_target, BackupInto::New).unwrap_err();
        assert_eq!(bak_err.code, ErrorCode::DurabilityFailed);
        assert!(!bak_target.exists(), "failed backup must not publish target directory");

        // After `acknowledge_restore()`, `backup_into` succeeds!
        db3.acknowledge_restore().unwrap();
        db3.backup_into(&bak_target, BackupInto::New).unwrap();
        assert!(bak_target.join("data.mdb").exists());
    }
}
