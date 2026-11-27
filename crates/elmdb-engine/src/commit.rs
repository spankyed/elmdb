use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use elmdb_storage::{
    Clock, CommitReceipt, DbFlags, DurabilityGate, DurabilityMode, GatedWriteTxn, KvCursor, KvRead, KvStore, MetaLock,
    RecoveryReport, SchemaEpochLock, SubDbHandle, SubDbPool, Vfs,
};
use elmdb_wire::{
    CanonicalValue, CommitReturnFlags, ElmdbError, EntityTarget, ErrorCode, Result, WireOp, decode_batch,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::catalog::{Cardinality, CompiledSchema, FieldDef, FieldKind, OnDeletePolicy, RawSchema, SchemaDiff};
use crate::ids::{EntityKey, FieldId, FieldMask, IndexId, RelKey, RelKindId, TypeId, TypeMask};
use crate::row::{
    MAX_EXT_ID_BYTES, PackedEdge, PackedRow, RowHeader, blob_field_ids_of, blob_key, decode_row, decode_row_header,
    decode_single_field, encode_row, make_trashed_flags,
};

/// Maximum cascade rounds for `beforeCommit` hooks before aborting with `HookCycleLimit`.
pub const MAX_HOOK_ROUNDS: usize = 8;
/// Number of recent commits retained in `commit_log` for `changedSince` & cross-process watch catch-up.
pub const COMMIT_LOG_WINDOW: u64 = 4096;
/// Maximum entity keys listed individually in a `CrossProcCommitSummary` before falling back to type-level.
pub const CROSS_PROC_ENTITY_CAP: usize = 64;

// `meta` sub-DB keys
const META_SCHEMA_KEY: &[u8] = b"schema";
const META_POOL_KEY: &[u8] = b"subdb_pool";
const META_COMMIT_SEQ_KEY: &[u8] = b"commit_seq";
const META_SCHEMA_EPOCH_KEY: &[u8] = b"schema_epoch";

// `sequences` sub-DB keys
const SEQ_ENTITY_KEY: &[u8] = b"\x01entity";
const SEQ_REL_KEY: &[u8] = b"\x01rel";
const SEQ_COUNTER_PREFIX: u8 = 0x02;
const MAX_COUNTER_NAME_BYTES: usize = 128;

// `adj` sub-DB 1-byte table tags
const TAG_ADJ_OUT: u8 = 0x01;
const TAG_ADJ_IN: u8 = 0x02;
const TAG_ADJ_BY_KIND: u8 = 0x03;
const TAG_REF_IN: u8 = 0x04;
const TAG_TYPE_LIST: u8 = 0x05;

// `commit_log` 1-byte table tags
const TAG_COMMIT_SUMMARY: u8 = 0x01;
const TAG_TOMBSTONE: u8 = 0x02;

/// Fixed primary & system LMDB sub-database handles.
#[derive(Debug, Clone, Copy)]
pub struct SystemDbs {
    pub meta: SubDbHandle,
    pub ext_id: SubDbHandle,
    pub entities: SubDbHandle,
    pub blobs: SubDbHandle,
    pub relations: SubDbHandle,
    pub rel_blobs: SubDbHandle,
    pub embeddings: SubDbHandle,
    pub sequences: SubDbHandle,
    pub commit_log: SubDbHandle,
    pub adj: SubDbHandle,
}

impl SystemDbs {
    pub fn open_or_create<S: KvStore, V: Vfs>(txn: &mut GatedWriteTxn<'_, S, V>) -> Result<Self> {
        Ok(Self {
            meta: txn.open_subdb("meta", DbFlags::DEFAULT)?,
            ext_id: txn.open_subdb("ext_id", DbFlags::DEFAULT)?,
            entities: txn.open_subdb("entities", DbFlags::INTEGER_KEY)?,
            blobs: txn.open_subdb("blobs", DbFlags::DEFAULT)?,
            relations: txn.open_subdb("relations", DbFlags::INTEGER_KEY)?,
            rel_blobs: txn.open_subdb("rel_blobs", DbFlags::DEFAULT)?,
            embeddings: txn.open_subdb("embeddings", DbFlags::DEFAULT)?,
            sequences: txn.open_subdb("sequences", DbFlags::DEFAULT)?,
            commit_log: txn.open_subdb("commit_log", DbFlags::DEFAULT)?,
            adj: txn.open_subdb("adj", DbFlags::DEFAULT)?,
        })
    }
}

/// Coalesced per-entity mutation passed to `ProjectionSink` (called at most once per entity per flush).
#[derive(Debug, Clone)]
pub struct EntityDelta {
    pub key: EntityKey,
    pub type_id: TypeId,
    pub before: Option<PackedRow>,
    pub after: Option<PackedRow>,
    pub changed_fields: FieldMask,
    pub liveness_changed: bool,
}

/// Coalesced per-edge mutation passed to `ProjectionSink`.
#[derive(Debug, Clone)]
pub struct EdgeDelta {
    pub key: RelKey,
    pub kind: RelKindId,
    pub before: Option<PackedEdge>,
    pub after: Option<PackedEdge>,
}

/// Extension seam allowing `elmdb-projections` to maintain secondary, order, text, trigram,
/// vector, and PageRank projections atomically inside the active write transaction.
pub trait ProjectionSink<S: KvStore, V: Vfs>: Send + Sync {
    fn apply_deltas(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        entity_deltas: &[EntityDelta],
        edge_deltas: &[EdgeDelta],
    ) -> Result<()>;

    fn on_schema_change(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        diff: &SchemaDiff,
    ) -> Result<()>;
}

/// No-op default projection sink (used when running `elmdb-engine` standalone before Phase 2 attaches).
#[derive(Debug, Default)]
pub struct NoopProjectionSink;

impl<S: KvStore, V: Vfs> ProjectionSink<S, V> for NoopProjectionSink {
    fn apply_deltas(
        &self,
        _txn: &mut GatedWriteTxn<'_, S, V>,
        _schema: &CompiledSchema,
        _dbs: &SystemDbs,
        _entity_deltas: &[EntityDelta],
        _edge_deltas: &[EdgeDelta],
    ) -> Result<()> {
        Ok(())
    }

    fn on_schema_change(
        &self,
        _txn: &mut GatedWriteTxn<'_, S, V>,
        _schema: &CompiledSchema,
        _dbs: &SystemDbs,
        _diff: &SchemaDiff,
    ) -> Result<()> {
        Ok(())
    }
}

/// Compact cross-process commit summary persisted in `commit_log` (last 4,096 commits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossProcCommitSummary {
    pub seq: u64,
    pub schema_epoch: u64,
    pub touched_types: Vec<TypeId>,
    pub touched_rel_kinds: Vec<RelKindId>,
    /// `Some(keys)` when <= 64 entities were touched; `None` signals type-level invalidation.
    pub touched_entities: Option<Vec<EntityKey>>,
}

/// In-process rich commit summary emitted after every commit (feeds `CommitRing` & watches).
#[derive(Debug, Clone)]
pub struct CommitDeltaSummary {
    pub seq: u64,
    pub txnid: u64,
    pub schema_epoch: u64,
    pub touched_types: TypeMask,
    pub touched_rel_kinds: Vec<RelKindId>,
    pub entity_deltas: Vec<EntityDelta>,
    pub edge_deltas: Vec<EdgeDelta>,
}

/// Result returned from `Engine::execute_batch`.
#[derive(Debug, Clone)]
pub struct BatchOutcome {
    pub commit_seq: u64,
    pub receipt: CommitReceipt,
    /// External IDs created in slot order (`0..create_count`).
    pub created_ids: Vec<String>,
    pub summary: Option<CommitDeltaSummary>,
}

/// Public decoded row returned from read APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRow {
    pub key: EntityKey,
    pub id: String,
    pub type_name: String,
    pub type_id: TypeId,
    pub trashed: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub trashed_at_ms: i64,
    pub fields: BTreeMap<String, CanonicalValue>,
}

/// Public decoded relation returned from read APIs.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEdge {
    pub key: RelKey,
    pub kind_name: String,
    pub kind_id: RelKindId,
    pub src: EntityKey,
    pub dst: EntityKey,
    pub weight: f64,
    pub created_at_ms: i64,
    pub info: Option<CanonicalValue>,
}

/// Synchronous `beforeCommit` hook registered with a `TypeMask` filter.
pub type BeforeCommitFn<S, V, C> = Arc<dyn Fn(&mut HookCtx<'_, S, V, C>) -> Result<()> + Send + Sync>;

pub struct BeforeCommitHook<S: KvStore, V: Vfs, C: Clock> {
    pub wants: TypeMask,
    pub callback: BeforeCommitFn<S, V, C>,
}

struct RegisteredHook<S: KvStore, V: Vfs, C: Clock> {
    id: u64,
    hook: BeforeCommitHook<S, V, C>,
}

/// Core v2 database engine parameterized over `KvStore`, `Vfs`, and `Clock`.
pub struct Engine<S: KvStore, V: Vfs, C: Clock> {
    gate: Arc<DurabilityGate<S, V>>,
    clock: C,
    dbs: SystemDbs,
    /// L1 lock guarding live `CompiledSchema` + `SubDbPool` snapshot.
    schema_state: SchemaEpochLock<SchemaState>,
    /// L4 lock guarding registered `beforeCommit` hooks.
    hooks: MetaLock<HookRegistry<S, V, C>>,
    projections: Arc<dyn ProjectionSink<S, V>>,
}

struct SchemaState {
    schema: Arc<CompiledSchema>,
    pool: SubDbPool,
}

struct HookRegistry<S: KvStore, V: Vfs, C: Clock> {
    next_id: u64,
    hooks: Vec<RegisteredHook<S, V, C>>,
}

impl<S: KvStore, V: Vfs, C: Clock> Default for HookRegistry<S, V, C> {
    fn default() -> Self {
        Self { next_id: 1, hooks: Vec::new() }
    }
}

/// Per-flush accumulator that folds multiple mutations to the same entity/edge into one delta.
#[derive(Clone)]
struct DeltaBatch {
    entities: BTreeMap<EntityKey, EntityDelta>,
    edges: BTreeMap<RelKey, EdgeDelta>,
    edge_endpoint_types: FxHashMap<RelKey, (Option<TypeId>, Option<TypeId>)>,
    touched_types: TypeMask,
    touched_rel_kinds: BTreeSet<RelKindId>,
    tombstones: Vec<(EntityKey, TypeId, String)>,
}

impl DeltaBatch {
    fn new() -> Self {
        Self {
            entities: BTreeMap::new(),
            edges: BTreeMap::new(),
            edge_endpoint_types: FxHashMap::default(),
            touched_types: TypeMask::empty(),
            touched_rel_kinds: BTreeSet::new(),
            tombstones: Vec::new(),
        }
    }

    fn record_entity(
        &mut self,
        key: EntityKey,
        type_id: TypeId,
        before: Option<PackedRow>,
        after: Option<PackedRow>,
        changed_fields: FieldMask,
        liveness_changed: bool,
    ) {
        match self.entities.get_mut(&key) {
            Some(existing) => {
                if existing.before.is_none() && after.is_none() {
                    // Created and destroyed in the same transaction -> net no-op.
                    self.entities.remove(&key);
                    self.tombstones.retain(|(k, _, _)| *k != key);
                } else {
                    existing.changed_fields.union_with(&changed_fields);
                    existing.liveness_changed |= liveness_changed;
                    existing.after = after;
                }
            }
            None => {
                if before.is_some() || after.is_some() {
                    self.entities
                        .insert(key, EntityDelta { key, type_id, before, after, changed_fields, liveness_changed });
                }
            }
        }
    }

    fn record_edge(
        &mut self,
        key: RelKey,
        kind: RelKindId,
        before: Option<PackedEdge>,
        after: Option<PackedEdge>,
        endpoint_types: (Option<TypeId>, Option<TypeId>),
    ) {
        match self.edges.get_mut(&key) {
            Some(existing) => {
                if existing.before.is_none() && after.is_none() {
                    // Linked and unlinked in the same transaction -> net no-op.
                    self.edges.remove(&key);
                    self.edge_endpoint_types.remove(&key);
                } else {
                    existing.after = after;
                    let entry = self.edge_endpoint_types.entry(key).or_insert((None, None));
                    if endpoint_types.0.is_some() {
                        entry.0 = endpoint_types.0;
                    }
                    if endpoint_types.1.is_some() {
                        entry.1 = endpoint_types.1;
                    }
                }
            }
            None => {
                if before.is_some() || after.is_some() {
                    self.edges.insert(key, EdgeDelta { key, kind, before, after });
                    self.edge_endpoint_types.insert(key, endpoint_types);
                }
            }
        }
    }

    fn recompute_touched(&mut self) {
        let mut types = TypeMask::empty();
        for d in self.entities.values() {
            types.insert(d.type_id);
        }
        let mut rels = BTreeSet::new();
        for (&rk, e) in &self.edges {
            rels.insert(e.kind);
            if let Some(&(src_ty, dst_ty)) = self.edge_endpoint_types.get(&rk) {
                if let Some(t) = src_ty {
                    types.insert(t);
                }
                if let Some(t) = dst_ty {
                    types.insert(t);
                }
            }
        }
        self.touched_types = types;
        self.touched_rel_kinds = rels;
    }

    fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.edges.is_empty()
    }
}

enum EngineRef<'a, S: KvStore, V: Vfs, C: Clock> {
    Borrowed(&'a Engine<S, V, C>),
    Owned(Arc<Engine<S, V, C>>),
}

impl<S: KvStore, V: Vfs, C: Clock> std::ops::Deref for EngineRef<'_, S, V, C> {
    type Target = Engine<S, V, C>;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(e) => e,
            Self::Owned(e) => e,
        }
    }
}

#[derive(Clone)]
struct SavedDeltaFrame {
    depth: u32,
    total_deltas: DeltaBatch,
    round_deltas: DeltaBatch,
    round_touched_types: TypeMask,
    pending_deltas: DeltaBatch,
    dirty: bool,
}

/// Snapshot of one `beforeCommit` round's deltas (used by host/JS hook runners).
#[derive(Debug, Clone)]
pub struct HookRoundSnapshot {
    pub commit_seq: u64,
    pub touched_types: TypeMask,
    pub entity_deltas: Vec<EntityDelta>,
    pub edge_deltas: Vec<EdgeDelta>,
}

/// Active write-transaction context exposed to `transact()` and `beforeCommit` hooks.
pub struct TxnCtx<'a, S: KvStore, V: Vfs, C: Clock> {
    engine: EngineRef<'a, S, V, C>,
    txn: GatedWriteTxn<'a, S, V>,
    schema: Arc<CompiledSchema>,
    commit_seq: u64,
    now_ms: i64,
    /// True once any entity, edge, counter, or external mutation was applied inside this transaction.
    dirty: bool,
    /// Accumulated deltas for the whole transaction (used for summary + `commit_log`).
    total_deltas: DeltaBatch,
    /// Deltas produced in the current round (delivered to `beforeCommit` hooks on the next round).
    round_deltas: DeltaBatch,
    /// Snapshot of deltas visible to the currently executing `beforeCommit` hook round.
    active_hook_deltas: Option<DeltaBatch>,
    /// Types touched in the current round (triggers `beforeCommit` cascade).
    round_touched_types: TypeMask,
    /// Pending un-flushed deltas (flushed to `ProjectionSink` after every statement / batch).
    pending_deltas: DeltaBatch,
    /// Stack of saved delta frames for explicit nested savepoints (`push_savepoint` / `abort_savepoint`).
    saved_frames: Vec<SavedDeltaFrame>,
}

/// Alias exposed to `beforeCommit` callbacks (same capabilities, inside a nested savepoint).
pub type HookCtx<'a, S, V, C> = TxnCtx<'a, S, V, C>;

impl<S: KvStore, V: Vfs, C: Clock> Engine<S, V, C> {
    /// Opens an engine with the default no-op projection sink.
    pub fn open(
        store: S,
        vfs: V,
        clock: C,
        dir: impl AsRef<Path>,
        mode: DurabilityMode,
    ) -> Result<(Self, RecoveryReport)> {
        Self::open_with_projections(store, vfs, clock, dir, mode, Arc::new(NoopProjectionSink))
    }

    /// Opens an engine with a custom `ProjectionSink` (e.g. `ProjectionRegistry`).
    pub fn open_with_projections(
        store: S,
        vfs: V,
        clock: C,
        dir: impl AsRef<Path>,
        mode: DurabilityMode,
        projections: Arc<dyn ProjectionSink<S, V>>,
    ) -> Result<(Self, RecoveryReport)> {
        Self::open_with_options(store, vfs, clock, dir, mode, projections, false)
    }

    /// Opens an engine with explicit `read_only` enforcement (never opens a write transaction when `read_only == true`).
    pub fn open_with_options(
        store: S,
        vfs: V,
        clock: C,
        dir: impl AsRef<Path>,
        mode: DurabilityMode,
        projections: Arc<dyn ProjectionSink<S, V>>,
        read_only: bool,
    ) -> Result<(Self, RecoveryReport)> {
        let (gate, report) = DurabilityGate::open(store, vfs, dir, mode)?;

        // Try read-only fast path first so reopening an existing DB never performs an
        // unnecessary write transaction / Strict-mode fsync on startup.
        let existing = {
            let ro = gate.begin_read()?;
            if let Some(dbs) = try_open_system_dbs_ro(&gate, &ro)?
                && let (Some(schema_bytes), Some(pool_bytes)) =
                    (ro.get(dbs.meta, META_SCHEMA_KEY)?, ro.get(dbs.meta, META_POOL_KEY)?)
            {
                let schema: CompiledSchema = serde_json::from_slice(schema_bytes)
                    .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt meta schema: {e}")))?;
                let pool = SubDbPool::decode(pool_bytes)?;
                Some((dbs, schema, pool))
            } else {
                None
            }
        };

        let (dbs, schema, pool) = match existing {
            Some(loaded) => loaded,
            None => {
                if read_only {
                    return Err(ElmdbError::new(
                        ErrorCode::NoDatabase,
                        "database directory is not initialized and cannot be initialized in read-only mode",
                    ));
                }
                let mut txn = gate.begin_write()?;
                let dbs = SystemDbs::open_or_create(&mut txn)?;
                let schema = match txn.get(dbs.meta, META_SCHEMA_KEY)? {
                    Some(bytes) => serde_json::from_slice::<CompiledSchema>(bytes)
                        .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt meta schema: {e}")))?,
                    None => {
                        let empty = CompiledSchema::empty();
                        let encoded = serde_json::to_vec(&empty)
                            .map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;
                        txn.put(dbs.meta, META_SCHEMA_KEY, &encoded)?;
                        txn.put(dbs.meta, META_SCHEMA_EPOCH_KEY, &empty.epoch.to_le_bytes())?;
                        empty
                    }
                };
                let pool = match txn.get(dbs.meta, META_POOL_KEY)? {
                    Some(bytes) => SubDbPool::decode(bytes)?,
                    None => {
                        let fresh = SubDbPool::default();
                        txn.put(dbs.meta, META_POOL_KEY, &fresh.encode())?;
                        fresh
                    }
                };
                if txn.get(dbs.meta, META_COMMIT_SEQ_KEY)?.is_none() {
                    txn.put(dbs.meta, META_COMMIT_SEQ_KEY, &0u64.to_le_bytes())?;
                }
                txn.commit()?;
                (dbs, schema, pool)
            }
        };

        let engine = Self {
            gate: Arc::new(gate),
            clock,
            dbs,
            schema_state: SchemaEpochLock::new(SchemaState { schema: Arc::new(schema), pool }),
            hooks: MetaLock::new(HookRegistry::default()),
            projections,
        };
        Ok((engine, report))
    }

    pub fn dbs(&self) -> SystemDbs {
        self.dbs
    }

    pub fn gate(&self) -> &DurabilityGate<S, V> {
        &self.gate
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// Returns the currently cached `Arc<CompiledSchema>`.
    pub fn schema(&self) -> Arc<CompiledSchema> {
        Arc::clone(&self.schema_state.read().schema)
    }

    /// Registers a synchronous `beforeCommit` hook filtered by `TypeMask`; returns its unregister ID.
    pub fn register_before_commit(&self, hook: BeforeCommitHook<S, V, C>) -> u64 {
        let mut reg = self.hooks.lock();
        let id = reg.next_id;
        reg.next_id += 1;
        reg.hooks.push(RegisteredHook { id, hook });
        id
    }

    pub fn unregister_before_commit(&self, id: u64) -> bool {
        let mut reg = self.hooks.lock();
        let before = reg.hooks.len();
        reg.hooks.retain(|h| h.id != id);
        reg.hooks.len() != before
    }

    pub fn has_matching_hooks(&self, touched: &TypeMask) -> bool {
        let reg = self.hooks.lock();
        reg.hooks.iter().any(|h| h.hook.wants.intersects(touched))
    }

    /// Flushes LMDB and writes the verified 4 KiB durability sidecar slot.
    pub fn sync(&self) -> Result<()> {
        self.gate.sync()?;
        Ok(())
    }

    /// Retries a pure write closure on `StorageFull` (`MDB_MAP_FULL`), doubling the LMDB map size
    /// under L2 `MapGate::write()` between attempts (when zero transactions are open).
    fn retry_on_map_full<T, F>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Result<T>,
    {
        const MAX_GROWTH_RETRIES: usize = 8;
        for attempt in 0..=MAX_GROWTH_RETRIES {
            match f() {
                Ok(v) => return Ok(v),
                Err(err) if err.code == ErrorCode::StorageFull && attempt < MAX_GROWTH_RETRIES => {
                    let before = self.gate.map_size();
                    let after = self.gate.grow_map(None)?;
                    if after <= before {
                        return Err(err);
                    }
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("loop returns on final attempt")
    }

    /// Compiles and installs a new schema, retiring dropped index slots via O(1) `truncate`
    /// without calling `mdb_dbi_close()` and without holding `SchemaEpochLock` (L1) across I/O or builds.
    pub fn set_schema(&self, raw: &RawSchema) -> Result<(Arc<CompiledSchema>, SchemaDiff)> {
        let (compiled, next_pool, diff) = self.retry_on_map_full(|| {
            let (mut txn, _) = self.begin_write_with_fresh_schema()?;
            let (cur_schema, mut next_pool) = load_schema_and_pool(&txn, &self.dbs)?;
            let (compiled, diff) = CompiledSchema::compile(raw, Some(&cur_schema), &mut next_pool)?;

            // Truncate retired sub-DB slots before recycling them into `meta.subdb_pool`.
            for &slot in &diff.retired_slots {
                let db = txn.open_subdb(&slot.name(), DbFlags::DEFAULT)?;
                txn.truncate(db)?;
            }
            // Purge any primary embeddings owned by removed/recreated indexes so `dbs.embeddings` never leaks!
            for &old_idx_id in &diff.removed_indexes {
                let prefix = old_idx_id.0.to_be_bytes();
                let mut to_del = Vec::new();
                {
                    let mut cur = txn.cursor(self.dbs.embeddings)?;
                    let mut entry = cur.seek_ge(&prefix)?;
                    while let Some((k, _)) = entry {
                        if !k.starts_with(&prefix) {
                            break;
                        }
                        to_del.push(k.to_vec());
                        entry = cur.next()?;
                    }
                }
                for k in to_del {
                    txn.del(self.dbs.embeddings, &k)?;
                }
            }
            // Pre-open all active index slots at root write txn so readers never need `mdb_dbi_open`.
            for def in &compiled.indexes {
                let _ = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
            }

            self.projections.on_schema_change(&mut txn, &compiled, &self.dbs, &diff)?;

            let seq = self.next_commit_seq(&txn)?;
            persist_schema_and_pool(&mut txn, &self.dbs, &compiled, &next_pool, seq)?;
            // Note: watchers must check `summary.schema_epoch` (not only `touched_types`) on catch-up.
            write_commit_summary(
                &mut txn,
                &self.dbs,
                CrossProcCommitSummary {
                    seq,
                    schema_epoch: compiled.epoch,
                    touched_types: Vec::new(),
                    touched_rel_kinds: Vec::new(),
                    touched_entities: None,
                },
                &[],
            )?;
            txn.commit()?;
            Ok((compiled, next_pool, diff))
        })?;

        let arc = Arc::new(compiled);
        self.publish_schema_monotonic(Arc::clone(&arc), next_pool);
        Ok((arc, diff))
    }

    /// O(1) metadata rename of a field (0 rows rewritten, 0 indexes rebuilt).
    pub fn rename_field(&self, type_name: &str, old_field: &str, new_field: &str) -> Result<Arc<CompiledSchema>> {
        self.apply_schema_rename(|schema| schema.rename_field(type_name, old_field, new_field))
    }

    /// O(1) metadata rename of a relation kind (0 edges rewritten).
    pub fn rename_relation(&self, old_rel: &str, new_rel: &str) -> Result<Arc<CompiledSchema>> {
        self.apply_schema_rename(|schema| schema.rename_relation(old_rel, new_rel))
    }

    /// O(1) metadata rename of a type (external IDs stay immutable by default; optional `rewrite_ids`
    /// rewrites `<old>-<suffix>` prefixes in `ext_id` and `PackedRow` headers only).
    pub fn rename_type(&self, old_type: &str, new_type: &str, rewrite_ids: bool) -> Result<Arc<CompiledSchema>> {
        let (next, pool) = self.retry_on_map_full(|| {
            let mut txn = self.gate.begin_write()?;
            let (cur_schema, pool) = load_schema_and_pool(&txn, &self.dbs)?;
            let type_id = cur_schema
                .type_by_name(old_type)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{old_type}'")))?
                .id;
            let next = cur_schema.rename_type(old_type, new_type)?;
            let seq = self.next_commit_seq(&txn)?;
            let now_ms = self.clock.now_ms();

            if rewrite_ids {
                let prefix = format!("{old_type}-");
                let keys = scan_type_keys(&txn, &self.dbs, type_id)?;
                for key in keys {
                    let raw = txn
                        .get(self.dbs.entities, &key.to_ne_bytes())?
                        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "type_list points to missing entity"))?
                        .to_vec();
                    let (hdr, _) = decode_row_header(&raw)?;
                    if let Some(suffix) = hdr.ext_id.strip_prefix(&prefix) {
                        let new_ext = format!("{new_type}-{suffix}");
                        if new_ext.len() > MAX_EXT_ID_BYTES {
                            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "rewritten external ID > 256 B"));
                        }
                        let mut row = decode_row(&raw, |fid| {
                            Ok(txn.get(self.dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned))
                        })?;
                        txn.del(self.dbs.ext_id, row.header.ext_id.as_bytes())?;
                        if !txn.put_no_overwrite(self.dbs.ext_id, new_ext.as_bytes(), &key.0.to_le_bytes())? {
                            return Err(ElmdbError::new(
                                ErrorCode::EntityAlreadyExists,
                                format!("rewritten ID '{new_ext}' collides with an existing entity"),
                            ));
                        }
                        row.header.ext_id = new_ext;
                        row.header.updated_seq = seq;
                        row.header.updated_at_ms = now_ms;
                        let enc = encode_row(&row)?;
                        txn.put(self.dbs.entities, &key.to_ne_bytes(), &enc.row_bytes)?;
                    }
                }
            }

            persist_schema_and_pool(&mut txn, &self.dbs, &next, &pool, seq)?;
            write_commit_summary(
                &mut txn,
                &self.dbs,
                CrossProcCommitSummary {
                    seq,
                    schema_epoch: next.epoch,
                    touched_types: vec![type_id],
                    touched_rel_kinds: Vec::new(),
                    touched_entities: None,
                },
                &[],
            )?;
            txn.commit()?;
            Ok((next, pool))
        })?;

        let arc = Arc::new(next);
        self.publish_schema_monotonic(Arc::clone(&arc), pool);
        Ok(arc)
    }

    fn apply_schema_rename<F>(&self, mut f: F) -> Result<Arc<CompiledSchema>>
    where
        F: FnMut(&CompiledSchema) -> Result<CompiledSchema>,
    {
        let (next, pool) = self.retry_on_map_full(|| {
            let mut txn = self.gate.begin_write()?;
            let (cur_schema, pool) = load_schema_and_pool(&txn, &self.dbs)?;
            let next = f(&cur_schema)?;
            let seq = self.next_commit_seq(&txn)?;
            persist_schema_and_pool(&mut txn, &self.dbs, &next, &pool, seq)?;
            write_commit_summary(
                &mut txn,
                &self.dbs,
                CrossProcCommitSummary {
                    seq,
                    schema_epoch: next.epoch,
                    touched_types: Vec::new(),
                    touched_rel_kinds: Vec::new(),
                    touched_entities: None,
                },
                &[],
            )?;
            txn.commit()?;
            Ok((next, pool))
        })?;
        let arc = Arc::new(next);
        self.publish_schema_monotonic(Arc::clone(&arc), pool);
        Ok(arc)
    }

    fn publish_schema_monotonic(&self, schema: Arc<CompiledSchema>, pool: SubDbPool) {
        let mut state = self.schema_state.write();
        if schema.epoch > state.schema.epoch {
            state.schema = schema;
            state.pool = pool;
        }
    }

    /// Acquires a write transaction and its exact matching `Arc<CompiledSchema>` with zero TOCTOU
    /// race and strict `L1 -> L2 -> L3` lock ordering (never holds L1 during the write transaction body!).
    fn begin_write_with_fresh_schema(&self) -> Result<(GatedWriteTxn<'_, S, V>, Arc<CompiledSchema>)> {
        {
            let guard = self.schema_state.read();
            let txn = self.gate.begin_write()?;
            let disk_epoch = read_schema_epoch(&txn, &self.dbs)?;
            if guard.schema.epoch == disk_epoch {
                let schema = Arc::clone(&guard.schema);
                drop(guard);
                return Ok((txn, schema));
            }
            // Drop L3 + L2 before taking L1.write() to uphold strict L1 -> L2 -> L3 order.
        }
        let mut state = self.schema_state.write();
        loop {
            let txn = self.gate.begin_write()?;
            let disk_epoch = read_schema_epoch(&txn, &self.dbs)?;
            if disk_epoch <= state.schema.epoch {
                let schema = Arc::clone(&state.schema);
                drop(state);
                return Ok((txn, schema));
            }
            let (schema, pool) = load_schema_and_pool(&txn, &self.dbs)?;
            // Drop L3+L2 write txn BEFORE pre-opening cross-process sub-DBs in an isolated committed RO txn
            // (under L2+L3 inside `ensure_subdbs_open`) so every DBI is permanently published in `MDB_env`
            // prior to `mdb_txn_begin()` of the write txn and NEVER races with any write txn's `mdb_dbi_open`!
            drop(txn);
            let slot_names: Vec<String> = schema.indexes.iter().map(|i| i.subdb_slot().name()).collect();
            let slot_refs: Vec<(&str, DbFlags)> = slot_names.iter().map(|s| (s.as_str(), DbFlags::DEFAULT)).collect();
            self.gate.ensure_subdbs_open(&slot_refs)?;
            state.schema = Arc::new(schema);
            state.pool = pool;
            // Loop to re-acquire the write txn and verify no concurrent process advanced `disk_epoch` again.
        }
    }

    /// Atomically compacts an index into a fresh staging `SubDbSlot` drawn from `SubDbPool`,
    /// swaps the index's active slot in `meta`, and truncates the retired slot in O(1) without
    /// ever calling `mdb_dbi_close()` (zero `MDB_DBS_FULL` leaks!).
    pub fn swap_index_slot<T, F>(&self, index_id: IndexId, mut f: F) -> Result<(T, CommitReceipt)>
    where
        F: FnMut(
            &mut GatedWriteTxn<'_, S, V>,
            &CompiledSchema,
            &SystemDbs,
            SubDbHandle, // current active sub-DB
            SubDbHandle, // clean staging sub-DB
        ) -> Result<T>,
    {
        let (out, receipt, next_schema, next_pool) = self.retry_on_map_full(|| {
            let mut txn = self.gate.begin_write()?;
            let (mut schema, mut pool) = load_schema_and_pool(&txn, &self.dbs)?;
            let idx_pos =
                schema.indexes.iter().position(|i| i.id == index_id).ok_or_else(|| {
                    ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0))
                })?;
            let old_slot = schema.indexes[idx_pos].subdb_slot();
            let staging_slot = pool.allocate()?;
            let old_db = txn.open_subdb(&old_slot.name(), DbFlags::DEFAULT)?;
            let staging_db = txn.open_subdb(&staging_slot.name(), DbFlags::DEFAULT)?;
            txn.truncate(staging_db)?;

            let out = f(&mut txn, &schema, &self.dbs, old_db, staging_db)?;

            // Flip active slot -> staging, O(1) truncate retired slot, recycle retired slot into pool!
            schema.indexes[idx_pos].slot = staging_slot.raw();
            schema.epoch = schema.epoch.saturating_add(1);
            txn.truncate(old_db)?;
            pool.recycle(old_slot)?;

            let seq = self.next_commit_seq(&txn)?;
            persist_schema_and_pool(&mut txn, &self.dbs, &schema, &pool, seq)?;
            write_commit_summary(
                &mut txn,
                &self.dbs,
                CrossProcCommitSummary {
                    seq,
                    schema_epoch: schema.epoch,
                    touched_types: Vec::new(),
                    touched_rel_kinds: Vec::new(),
                    touched_entities: None,
                },
                &[],
            )?;
            let receipt = txn.commit()?;
            Ok((out, receipt, schema, pool))
        })?;
        self.publish_schema_monotonic(Arc::new(next_schema), next_pool);
        Ok((out, receipt))
    }

    /// Runs a lock-order-safe maintenance write transaction (used by chunked index builders,
    /// `rebuild()`, vector drain, and ping-pong compaction) with zero schema-epoch TOCTOU and
    /// automatic LMDB map growth retry.
    pub fn run_maintenance_write<T, F>(&self, mut f: F) -> Result<(T, CommitReceipt)>
    where
        F: FnMut(&mut GatedWriteTxn<'_, S, V>, &Arc<CompiledSchema>, &SystemDbs) -> Result<T>,
    {
        self.retry_on_map_full(|| {
            let (mut txn, schema) = self.begin_write_with_fresh_schema()?;
            let out = f(&mut txn, &schema, &self.dbs)?;
            let receipt = txn.commit()?;
            Ok((out, receipt))
        })
    }

    /// Executes a binary `elmdb-wire` batch frame in one atomic commit.
    pub fn execute_wire_batch(&self, frame: &[u8]) -> Result<BatchOutcome> {
        let decoded = decode_batch(frame)?;
        self.execute_batch_with_flags(&decoded.ops, decoded.return_flags)
    }

    /// Executes a slice of `WireOp`s (supporting `EntityTarget::BatchSlot` intra-batch refs)
    /// in one atomic commit through `CommitPipeline` -> `ProjectionSink` -> `beforeCommit` cascade.
    pub fn execute_batch(&self, ops: &[WireOp]) -> Result<BatchOutcome> {
        self.execute_batch_with_flags(ops, CommitReturnFlags::RESULTS.union(CommitReturnFlags::FULL_CHANGESET))
    }

    pub fn execute_batch_with_flags(&self, ops: &[WireOp], flags: CommitReturnFlags) -> Result<BatchOutcome> {
        self.retry_on_map_full(|| {
            let (txn, schema) = self.begin_write_with_fresh_schema()?;
            let commit_seq = self.next_commit_seq(&txn)?;
            let now_ms = self.clock.now_ms();

            let mut ctx = TxnCtx {
                engine: EngineRef::Borrowed(self),
                txn,
                schema,
                commit_seq,
                now_ms,
                dirty: false,
                total_deltas: DeltaBatch::new(),
                round_deltas: DeltaBatch::new(),
                active_hook_deltas: None,
                round_touched_types: TypeMask::empty(),
                pending_deltas: DeltaBatch::new(),
                saved_frames: Vec::new(),
            };

            let mut batch_slots: Vec<(EntityKey, String)> = Vec::new();
            for op in ops {
                ctx.apply_op(op, &mut batch_slots)?;
            }
            ctx.flush_projections()?;
            ctx.run_before_commit_cascade()?;
            ctx.finalize_commit_log()?;

            let created_ids: Vec<String> = batch_slots.into_iter().map(|(_, id)| id).collect();
            let (receipt, summary_out) = ctx.commit_with_summary(flags.contains(CommitReturnFlags::FULL_CHANGESET))?;
            Ok(BatchOutcome { commit_seq, receipt, created_ids, summary: summary_out })
        })
    }

    /// Begins an owned `'static` (but `!Send`) write `TxnCtx` for callback-scoped synchronous
    /// `Database::transact(fn)` with native nested savepoints and full read-your-own-writes.
    pub fn begin_owned_txn(self: &Arc<Self>) -> Result<TxnCtx<'static, S, V, C>>
    where
        S: 'static,
        V: 'static,
        C: 'static,
    {
        let (txn, schema) = self.begin_write_owned_with_fresh_schema()?;
        let commit_seq = self.next_commit_seq(&txn)?;
        let now_ms = self.clock.now_ms();
        Ok(TxnCtx {
            engine: EngineRef::Owned(Arc::clone(self)),
            txn,
            schema,
            commit_seq,
            now_ms,
            dirty: false,
            total_deltas: DeltaBatch::new(),
            round_deltas: DeltaBatch::new(),
            active_hook_deltas: None,
            round_touched_types: TypeMask::empty(),
            pending_deltas: DeltaBatch::new(),
            saved_frames: Vec::new(),
        })
    }

    fn begin_write_owned_with_fresh_schema(&self) -> Result<(GatedWriteTxn<'static, S, V>, Arc<CompiledSchema>)>
    where
        S: 'static,
        V: 'static,
    {
        {
            let guard = self.schema_state.read();
            let txn = self.gate.begin_write_owned()?;
            let disk_epoch = read_schema_epoch(&txn, &self.dbs)?;
            if guard.schema.epoch == disk_epoch {
                let schema = Arc::clone(&guard.schema);
                drop(guard);
                return Ok((txn, schema));
            }
        }
        let mut state = self.schema_state.write();
        loop {
            let txn = self.gate.begin_write_owned()?;
            let disk_epoch = read_schema_epoch(&txn, &self.dbs)?;
            if disk_epoch <= state.schema.epoch {
                let schema = Arc::clone(&state.schema);
                drop(state);
                return Ok((txn, schema));
            }
            let (schema, pool) = load_schema_and_pool(&txn, &self.dbs)?;
            drop(txn);
            let slot_names: Vec<String> = schema.indexes.iter().map(|i| i.subdb_slot().name()).collect();
            let slot_refs: Vec<(&str, DbFlags)> = slot_names.iter().map(|s| (s.as_str(), DbFlags::DEFAULT)).collect();
            self.gate.ensure_subdbs_open(&slot_refs)?;
            state.schema = Arc::new(schema);
            state.pool = pool;
        }
    }

    /// Restores persisted entity and relation timestamps during lossless `dump::load` without
    /// altering field payloads, edges, or derived projections.
    pub fn restore_loaded_timestamps(
        &self,
        entity_timestamps: &[(String, i64, i64, i64)],
        edge_timestamps: &[(String, String, String, i64)],
    ) -> Result<()> {
        use std::collections::VecDeque;

        if entity_timestamps.is_empty() && edge_timestamps.is_empty() {
            return Ok(());
        }
        self.retry_on_map_full(|| {
            let (mut txn, schema) = self.begin_write_with_fresh_schema()?;
            let dbs = self.dbs;

            for (ext_id, created_at_ms, updated_at_ms, trashed_at_ms) in entity_timestamps {
                let Some(key) = lookup_ext_id_in_txn(&txn, &dbs, ext_id)? else {
                    continue;
                };
                let Some(raw) = txn.get(dbs.entities, &key.to_ne_bytes())? else {
                    continue;
                };
                let mut packed =
                    decode_row(raw, |fid| Ok(txn.get(dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned)))?;
                packed.header.created_at_ms = *created_at_ms;
                packed.header.updated_at_ms = *updated_at_ms;
                packed.header.trashed_at_ms = *trashed_at_ms;
                let enc = encode_row(&packed)?;
                txn.put(dbs.entities, &key.to_ne_bytes(), &enc.row_bytes)?;
            }

            if !edge_timestamps.is_empty() {
                let mut queues: FxHashMap<(RelKindId, EntityKey, EntityKey), VecDeque<i64>> = FxHashMap::default();
                for (rel_kind, src_ext, dst_ext, created_at_ms) in edge_timestamps {
                    let Some(rdef) = schema.relations.iter().find(|r| r.name == *rel_kind) else {
                        continue;
                    };
                    let Some(src) = lookup_ext_id_in_txn(&txn, &dbs, src_ext)? else {
                        continue;
                    };
                    let Some(dst) = lookup_ext_id_in_txn(&txn, &dbs, dst_ext)? else {
                        continue;
                    };
                    queues.entry((rdef.id, src, dst)).or_default().push_back(*created_at_ms);
                }
                let mut updates: Vec<(RelKey, Vec<u8>)> = Vec::new();
                {
                    let mut cur = txn.cursor(dbs.relations)?;
                    let mut item = cur.first()?;
                    while let Some((k, v)) = item {
                        if k.len() == 8 {
                            let rk = RelKey::from_ne_bytes(k.try_into().expect("8"));
                            let mut edge = PackedEdge::decode(rk, v)?;
                            if let Some(q) = queues.get_mut(&(edge.kind, edge.src, edge.dst))
                                && let Some(ts) = q.pop_front()
                            {
                                edge.created_at_ms = ts;
                                updates.push((rk, edge.encode()));
                            }
                        }
                        item = cur.next()?;
                    }
                }
                for (rk, bytes) in updates {
                    txn.put(dbs.relations, &rk.to_ne_bytes(), &bytes)?;
                }
            }

            txn.commit()?;
            Ok(())
        })
    }

    /// Executes a synchronous read-modify-write transaction with native LMDB nested savepoints
    /// and a final bounded `beforeCommit` cascade.
    pub fn transact<T, F>(&self, f: F) -> Result<(T, CommitReceipt)>
    where
        F: FnOnce(&mut TxnCtx<'_, S, V, C>) -> Result<T>,
    {
        let (txn, schema) = self.begin_write_with_fresh_schema()?;
        let commit_seq = self.next_commit_seq(&txn)?;
        let now_ms = self.clock.now_ms();

        let mut ctx = TxnCtx {
            engine: EngineRef::Borrowed(self),
            txn,
            schema,
            commit_seq,
            now_ms,
            dirty: false,
            total_deltas: DeltaBatch::new(),
            round_deltas: DeltaBatch::new(),
            active_hook_deltas: None,
            round_touched_types: TypeMask::empty(),
            pending_deltas: DeltaBatch::new(),
            saved_frames: Vec::new(),
        };

        let value = f(&mut ctx)?;
        ctx.flush_projections()?;
        ctx.run_before_commit_cascade()?;
        ctx.finalize_commit_log()?;
        let (receipt, _) = ctx.commit_with_summary(false)?;
        Ok((value, receipt))
    }

    /// Atomically increments named transactional counter `name` by 1 and returns the new value (`1, 2, ...`).
    pub fn next_counter(&self, name: &str) -> Result<u64> {
        self.retry_on_map_full(|| {
            let mut txn = self.gate.begin_write()?;
            let next = next_counter_in_txn(&mut txn, &self.dbs, name)?;
            txn.commit()?;
            Ok(next)
        })
    }

    /// Reads the current value of named counter `name` (`0` if never incremented).
    pub fn get_counter(&self, name: &str) -> Result<u64> {
        let ro = self.gate.begin_read()?;
        get_counter_in_txn(&ro, &self.dbs, name)
    }

    /// Sets named transactional counter `name` to `value`.
    pub fn set_counter(&self, name: &str, value: u64) -> Result<()> {
        self.retry_on_map_full(|| {
            let mut txn = self.gate.begin_write()?;
            set_counter_in_txn(&mut txn, &self.dbs, name, value)?;
            txn.commit()?;
            Ok(())
        })
    }

    /// Lists all named counters (for lossless `dump()`).
    pub fn list_counters(&self) -> Result<BTreeMap<String, u64>> {
        let ro = self.gate.begin_read()?;
        list_counters_in_txn(&ro, &self.dbs)
    }

    /// Reads a complete entity row by external ID using a single pinned MVCC snapshot.
    pub fn get_row(&self, ext_id: &str) -> Result<Option<StoredRow>> {
        let (ro, schema) = self.begin_read_pinned()?;
        let Some(key_bytes) = ro.get(self.dbs.ext_id, ext_id.as_bytes())? else {
            return Ok(None);
        };
        let key = EntityKey(u64::from_le_bytes(
            key_bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "corrupt ext_id key bytes"))?,
        ));
        read_stored_row(&ro, &schema, &self.dbs, key)
    }

    /// Reads a complete entity row by internal `EntityKey(u64)`.
    pub fn get_row_by_key(&self, key: EntityKey) -> Result<Option<StoredRow>> {
        let (ro, schema) = self.begin_read_pinned()?;
        read_stored_row(&ro, &schema, &self.dbs, key)
    }

    /// Reads a single declared field via O(log F) binary search without decoding sibling fields or blobs.
    pub fn get_field(&self, ext_id: &str, field_name: &str) -> Result<Option<CanonicalValue>> {
        let (ro, schema) = self.begin_read_pinned()?;
        read_single_field(&ro, &schema, &self.dbs, ext_id, field_name)
    }

    /// Resolves an internal `EntityKey(u64)` to its external string ID (zero row decompression).
    pub fn resolve_ext_id(&self, key: EntityKey) -> Result<Option<String>> {
        let (ro, _) = self.begin_read_pinned()?;
        let Some(raw) = ro.get(self.dbs.entities, &key.to_ne_bytes())? else {
            return Ok(None);
        };
        Ok(Some(decode_row_header(raw)?.0.ext_id))
    }

    /// Lists all entities of `type_name` (active or archived).
    pub fn list_by_type(&self, type_name: &str, include_trashed: bool) -> Result<Vec<StoredRow>> {
        let (ro, schema) = self.begin_read_pinned()?;
        let tdef = schema
            .type_by_name_any(type_name)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{type_name}'")))?;
        let keys = scan_type_keys(&ro, &self.dbs, tdef.id)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(row) = read_stored_row(&ro, &schema, &self.dbs, key)?
                && (include_trashed || !row.trashed)
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Lists outgoing relations from `ext_id`, optionally filtered by `rel_kind`.
    pub fn relations_out(&self, ext_id: &str, rel_kind: Option<&str>) -> Result<Vec<StoredEdge>> {
        let (ro, schema) = self.begin_read_pinned()?;
        read_relations_for_ext_id(&ro, &schema, &self.dbs, TAG_ADJ_OUT, ext_id, rel_kind)
    }

    /// Lists incoming relations to `ext_id`, optionally filtered by `rel_kind`.
    pub fn relations_in(&self, ext_id: &str, rel_kind: Option<&str>) -> Result<Vec<StoredEdge>> {
        let (ro, schema) = self.begin_read_pinned()?;
        read_relations_for_ext_id(&ro, &schema, &self.dbs, TAG_ADJ_IN, ext_id, rel_kind)
    }

    /// Reads persisted cross-process commit summaries with `seq > since_seq`.
    /// Returns `Ok(None)` if `> 4,096` commits elapsed and the caller must perform full invalidation.
    pub fn read_commit_summaries_since(&self, since_seq: u64) -> Result<Option<Vec<CrossProcCommitSummary>>> {
        let (ro, _) = self.begin_read_pinned()?;
        let (_, summaries) = read_commit_summaries_in_txn(&ro, &self.dbs, since_seq)?;
        Ok(summaries)
    }

    /// Opens a read-only transaction pinned to the exact `Arc<CompiledSchema>` for its MVCC snapshot.
    ///
    /// Monotonicity invariant: only advances shared `schema_state` when `disk_epoch > cached.epoch`.
    /// If a concurrent writer already advanced `schema_state` ahead of `ro`'s snapshot
    /// (`disk_epoch < cached.epoch`), deserializes the snapshot's historical schema locally so
    /// shared state never regresses backwards!
    pub fn begin_read_pinned(&self) -> Result<(S::RoTxn<'_>, Arc<CompiledSchema>)> {
        loop {
            // Snapshot cached schema under L1 BEFORE acquiring `ro` (L2 MapGate read) so we
            // never acquire L1 while holding L2 (strict L1 -> L2 -> L3 -> L4 lock order).
            let cached = Arc::clone(&self.schema_state.read().schema);
            let ro = self.gate.begin_read()?;
            let disk_epoch = read_schema_epoch(&ro, &self.dbs)?;
            if cached.epoch == disk_epoch {
                return Ok((ro, cached));
            }
            if disk_epoch < cached.epoch {
                let (historical, _) = load_schema_and_pool(&ro, &self.dbs)?;
                return Ok((ro, Arc::new(historical)));
            }
            // Cross-process schema advance (`disk_epoch > cached.epoch`): drop `ro` (L2 read) FIRST
            // so strict `L1 -> L2 -> L3` order is preserved, then pre-open any newly-created index
            // sub-DBs via `ensure_subdbs_open` (L2+L3) before updating `schema_state` (L1) and re-opening `RoTxn`!
            let (schema, pool) = load_schema_and_pool(&ro, &self.dbs)?;
            drop(ro);
            let slot_names: Vec<String> = schema.indexes.iter().map(|i| i.subdb_slot().name()).collect();
            let slot_refs: Vec<(&str, DbFlags)> = slot_names.iter().map(|s| (s.as_str(), DbFlags::DEFAULT)).collect();
            self.gate.ensure_subdbs_open(&slot_refs)?;
            {
                let mut state = self.schema_state.write();
                if disk_epoch > state.schema.epoch {
                    state.schema = Arc::new(schema);
                    state.pool = pool;
                }
            }
            // Next iteration opens a fresh `RoTxn` whose LMDB `mt_numdbs` includes all pre-opened DBIs!
        }
    }

    fn next_commit_seq<R: KvRead>(&self, txn: &R) -> Result<u64> {
        Ok(read_commit_seq(txn, &self.dbs)?.saturating_add(1))
    }
}

impl<'a, S: KvStore, V: Vfs, C: Clock> TxnCtx<'a, S, V, C> {
    pub fn schema(&self) -> &Arc<CompiledSchema> {
        &self.schema
    }

    /// Returns coalesced entity deltas for the current `beforeCommit` hook round (or the whole txn outside hooks).
    pub fn entity_deltas(&self) -> Vec<&EntityDelta> {
        self.active_hook_deltas.as_ref().unwrap_or(&self.total_deltas).entities.values().collect()
    }

    /// Returns coalesced edge deltas for the current `beforeCommit` hook round (or the whole txn outside hooks).
    pub fn edge_deltas(&self) -> Vec<&EdgeDelta> {
        self.active_hook_deltas.as_ref().unwrap_or(&self.total_deltas).edges.values().collect()
    }

    /// Resolves an internal `EntityKey` to its external string ID within this transaction.
    pub fn resolve_ext_id(&self, key: EntityKey) -> Result<Option<String>> {
        let Some(raw) = self.txn.get(self.engine.dbs.entities, &key.to_ne_bytes())? else {
            return Ok(None);
        };
        Ok(Some(decode_row_header(raw)?.0.ext_id))
    }

    /// Pending monotonic commit sequence number assigned to this write transaction.
    pub fn commit_seq(&self) -> u64 {
        self.commit_seq
    }

    /// Returns `true` if any write (entity, edge, counter, or external mutation) occurred in this transaction.
    pub fn is_dirty(&self) -> bool {
        self.dirty || !self.pending_deltas.is_empty() || !self.total_deltas.is_empty()
    }

    /// Marks this transaction dirty (e.g. after an out-of-band embedding write on `txn_mut()`).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns a read/write reference to the underlying `GatedWriteTxn` for RYOW queries and projections.
    pub fn txn(&self) -> &GatedWriteTxn<'a, S, V> {
        &self.txn
    }

    pub fn txn_mut(&mut self) -> &mut GatedWriteTxn<'a, S, V> {
        &mut self.txn
    }

    pub fn dbs(&self) -> SystemDbs {
        self.engine.dbs
    }

    /// Atomically increments named counter `name` inside this transaction (rolls back on savepoint/txn abort).
    pub fn next_counter(&mut self, name: &str) -> Result<u64> {
        let next = next_counter_in_txn(&mut self.txn, &self.engine.dbs, name)?;
        self.dirty = true;
        Ok(next)
    }

    /// Reads named counter `name` inside this transaction (full read-your-own-writes).
    pub fn get_counter(&self, name: &str) -> Result<u64> {
        get_counter_in_txn(&self.txn, &self.engine.dbs, name)
    }

    /// Sets named counter `name` inside this transaction (rolls back on savepoint/txn abort).
    pub fn set_counter(&mut self, name: &str, value: u64) -> Result<()> {
        set_counter_in_txn(&mut self.txn, &self.engine.dbs, name, value)?;
        self.dirty = true;
        Ok(())
    }

    /// Executes a batch of wire ops inside an atomic nested savepoint, supporting intra-batch `BatchSlot` refs,
    /// and immediately flushes projections so subsequent reads/queries in the same transaction see fresh indexes.
    /// Any mid-batch or projection error (e.g. `UniqueViolation`) rolls back both LMDB writes and delta accumulators.
    pub fn exec_batch_and_flush(&mut self, ops: &[WireOp]) -> Result<Vec<String>> {
        self.savepoint(|ctx| {
            let mut slots = Vec::new();
            for op in ops {
                ctx.apply_op(op, &mut slots)?;
            }
            ctx.flush_projections()?;
            Ok(slots.into_iter().map(|(_, ext_id)| ext_id).collect())
        })
    }

    /// Opens an explicit native LMDB nested savepoint and snapshots in-memory delta state.
    pub fn push_savepoint(&mut self) -> Result<u32> {
        let depth = self.txn.begin_savepoint()?;
        self.saved_frames.push(SavedDeltaFrame {
            depth,
            total_deltas: self.total_deltas.clone(),
            round_deltas: self.round_deltas.clone(),
            round_touched_types: self.round_touched_types.clone(),
            pending_deltas: self.pending_deltas.clone(),
            dirty: self.dirty,
        });
        Ok(depth)
    }

    /// Commits (merges into parent) the top-most explicit nested savepoint without firing `beforeCommit`.
    /// On any error, aborts the savepoint and restores delta/dirty state so no orphan savepoint leaks.
    pub fn commit_savepoint(&mut self, depth: u32) -> Result<()> {
        if let Err(err) = self.flush_projections() {
            self.abort_savepoint(depth);
            return Err(err);
        }
        if let Err(err) = self.txn.commit_savepoint(depth) {
            self.abort_savepoint(depth);
            return Err(err.into());
        }
        if let Some(pos) = self.saved_frames.iter().rposition(|f| f.depth == depth) {
            self.saved_frames.truncate(pos);
        }
        Ok(())
    }

    /// Aborts (rolls back) the top-most explicit nested savepoint and restores delta/dirty state cleanly.
    pub fn abort_savepoint(&mut self, depth: u32) {
        while self.txn.savepoint_depth() >= depth {
            let d = self.txn.savepoint_depth();
            let _ = self.txn.abort_savepoint(d);
        }
        if let Some(pos) = self.saved_frames.iter().rposition(|f| f.depth == depth) {
            let frame = self.saved_frames[pos].clone();
            self.saved_frames.truncate(pos);
            self.total_deltas = frame.total_deltas;
            self.round_deltas = frame.round_deltas;
            self.round_touched_types = frame.round_touched_types;
            self.pending_deltas = frame.pending_deltas;
            self.dirty = frame.dirty;
        }
    }

    /// Takes the current round's pending deltas for a host-driven `beforeCommit` hook round.
    /// Returns `Ok(None)` when no types were touched in the latest round.
    pub fn take_hook_round(&mut self) -> Result<Option<HookRoundSnapshot>> {
        self.flush_projections()?;
        if self.round_touched_types.is_empty() {
            return Ok(None);
        }
        let touched_types = std::mem::replace(&mut self.round_touched_types, TypeMask::empty());
        let deltas = std::mem::replace(&mut self.round_deltas, DeltaBatch::new());
        let entity_deltas: Vec<EntityDelta> = deltas.entities.values().cloned().collect();
        let edge_deltas: Vec<EdgeDelta> = deltas.edges.values().cloned().collect();
        self.active_hook_deltas = Some(deltas);
        Ok(Some(HookRoundSnapshot { commit_seq: self.commit_seq, touched_types, entity_deltas, edge_deltas }))
    }

    /// Runs registered Rust `Engine` `beforeCommit` hooks matching `touched_types` for the current round.
    pub fn run_rust_hooks_for_round(&mut self, touched_types: &TypeMask) -> Result<()> {
        let matching: Vec<BeforeCommitFn<S, V, C>> = {
            let reg = self.engine.hooks.lock();
            reg.hooks
                .iter()
                .filter(|h| h.hook.wants.intersects(touched_types))
                .map(|h| Arc::clone(&h.hook.callback))
                .collect()
        };
        for cb in matching {
            cb(self)?;
        }
        Ok(())
    }

    /// Clears `active_hook_deltas` at the end of a host-driven hook round.
    pub fn finish_hook_round(&mut self) -> Result<()> {
        self.active_hook_deltas = None;
        self.flush_projections()
    }

    /// Finalizes `commit_log` and commits the root transaction, returning `(commit_seq, receipt, summary)`.
    /// If `is_dirty()` is false, aborts without bumping `commit_seq` or `txnid` and returns `Ok(None)`.
    pub fn finish_and_commit(
        mut self,
        want_summary: bool,
    ) -> Result<Option<(u64, CommitReceipt, Option<CommitDeltaSummary>)>> {
        self.flush_projections()?;
        if !self.is_dirty() {
            self.txn.abort();
            return Ok(None);
        }
        self.finalize_commit_log()?;
        let seq = self.commit_seq;
        let (receipt, summary) = self.commit_with_summary(want_summary)?;
        Ok(Some((seq, receipt, summary)))
    }

    /// Executes a batch of wire ops inside this active transaction, supporting intra-batch `BatchSlot` refs.
    pub fn exec_batch(&mut self, ops: &[WireOp]) -> Result<Vec<String>> {
        let mut slots = Vec::new();
        for op in ops {
            self.apply_op(op, &mut slots)?;
        }
        Ok(slots.into_iter().map(|(_, ext_id)| ext_id).collect())
    }

    pub fn get_row(&self, ext_id: &str) -> Result<Option<StoredRow>> {
        let Some(key_bytes) = self.txn.get(self.engine.dbs.ext_id, ext_id.as_bytes())? else {
            return Ok(None);
        };
        let key = EntityKey(decode_u64_le(key_bytes, "ext_id")?);
        read_stored_row(&self.txn, &self.schema, &self.engine.dbs, key)
    }

    pub fn get_field(&self, ext_id: &str, field_name: &str) -> Result<Option<CanonicalValue>> {
        read_single_field(&self.txn, &self.schema, &self.engine.dbs, ext_id, field_name)
    }

    pub fn relations_out(&self, ext_id: &str, rel_kind: Option<&str>) -> Result<Vec<StoredEdge>> {
        read_relations_for_ext_id(&self.txn, &self.schema, &self.engine.dbs, TAG_ADJ_OUT, ext_id, rel_kind)
    }

    pub fn relations_in(&self, ext_id: &str, rel_kind: Option<&str>) -> Result<Vec<StoredEdge>> {
        read_relations_for_ext_id(&self.txn, &self.schema, &self.engine.dbs, TAG_ADJ_IN, ext_id, rel_kind)
    }

    pub fn list_by_type(&self, type_name: &str, include_trashed: bool) -> Result<Vec<StoredRow>> {
        let tdef = self
            .schema
            .type_by_name_any(type_name)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{type_name}'")))?;
        let keys = scan_type_keys(&self.txn, &self.engine.dbs, tdef.id)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(row) = read_stored_row(&self.txn, &self.schema, &self.engine.dbs, key)?
                && (include_trashed || !row.trashed)
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Executes a single `WireOp` atomically inside a native nested savepoint and flushes its
    /// coalesced delta to `ProjectionSink` so subsequent reads inside the same transaction see fresh indexes.
    pub fn exec(&mut self, op: &WireOp) -> Result<Option<String>> {
        self.savepoint(|ctx| {
            let mut slots = Vec::new();
            ctx.apply_op(op, &mut slots)?;
            ctx.flush_projections()?;
            Ok(slots.into_iter().next().map(|(_, id)| id))
        })
    }

    /// Executes `f` inside a native LMDB nested savepoint, rolling back BOTH LMDB state AND
    /// in-memory delta/hook accumulators cleanly on `Err`.
    pub fn savepoint<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        let depth = self.push_savepoint()?;
        match f(self) {
            Ok(v) => {
                self.commit_savepoint(depth)?;
                Ok(v)
            }
            Err(err) => {
                self.abort_savepoint(depth);
                Err(err)
            }
        }
    }

    fn flush_projections(&mut self) -> Result<()> {
        if self.pending_deltas.is_empty() {
            self.pending_deltas = DeltaBatch::new();
            return Ok(());
        }
        self.dirty = true;
        let batch = std::mem::replace(&mut self.pending_deltas, DeltaBatch::new());
        let entity_vec: Vec<EntityDelta> = batch.entities.values().cloned().collect();
        let edge_vec: Vec<EdgeDelta> = batch.edges.values().cloned().collect();

        self.engine.projections.apply_deltas(&mut self.txn, &self.schema, &self.engine.dbs, &entity_vec, &edge_vec)?;

        for d in entity_vec {
            self.round_deltas.record_entity(
                d.key,
                d.type_id,
                d.before.clone(),
                d.after.clone(),
                d.changed_fields.clone(),
                d.liveness_changed,
            );
            self.total_deltas.record_entity(d.key, d.type_id, d.before, d.after, d.changed_fields, d.liveness_changed);
        }
        for e in edge_vec {
            let ep = batch.edge_endpoint_types.get(&e.key).copied().unwrap_or((None, None));
            self.round_deltas.record_edge(e.key, e.kind, e.before.clone(), e.after.clone(), ep);
            self.total_deltas.record_edge(e.key, e.kind, e.before, e.after, ep);
        }
        self.total_deltas.tombstones.extend(batch.tombstones);
        self.round_deltas.recompute_touched();
        self.total_deltas.recompute_touched();
        self.round_touched_types = self.round_deltas.touched_types.clone();
        Ok(())
    }

    fn run_before_commit_cascade(&mut self) -> Result<()> {
        for round in 0..=MAX_HOOK_ROUNDS {
            if self.round_touched_types.is_empty() {
                self.active_hook_deltas = None;
                return Ok(());
            }
            let matching: Vec<BeforeCommitFn<S, V, C>> = {
                let reg = self.engine.hooks.lock();
                reg.hooks
                    .iter()
                    .filter(|h| h.hook.wants.intersects(&self.round_touched_types))
                    .map(|h| Arc::clone(&h.hook.callback))
                    .collect()
            };
            if matching.is_empty() {
                self.active_hook_deltas = None;
                return Ok(());
            }
            if round == MAX_HOOK_ROUNDS {
                self.active_hook_deltas = None;
                return Err(ElmdbError::new(
                    ErrorCode::HookCycleLimit,
                    format!("beforeCommit hook cascade exceeded {MAX_HOOK_ROUNDS} rounds"),
                ));
            }
            self.round_touched_types = TypeMask::empty();
            self.active_hook_deltas = Some(std::mem::replace(&mut self.round_deltas, DeltaBatch::new()));
            let depth = self.txn.begin_savepoint()?;
            for cb in matching {
                if let Err(err) = cb(self) {
                    self.active_hook_deltas = None;
                    while self.txn.savepoint_depth() >= depth {
                        let d = self.txn.savepoint_depth();
                        let _ = self.txn.abort_savepoint(d);
                    }
                    return Err(err);
                }
            }
            self.active_hook_deltas = None;
            self.flush_projections()?;
            self.txn.commit_savepoint(depth)?;
        }
        Ok(())
    }

    fn finalize_commit_log(&mut self) -> Result<CrossProcCommitSummary> {
        self.txn.put(self.engine.dbs.meta, META_COMMIT_SEQ_KEY, &self.commit_seq.to_le_bytes())?;
        let touched_entities = if self.total_deltas.entities.len() <= CROSS_PROC_ENTITY_CAP {
            Some(self.total_deltas.entities.keys().copied().collect())
        } else {
            None
        };
        let summary = CrossProcCommitSummary {
            seq: self.commit_seq,
            schema_epoch: self.schema.epoch,
            touched_types: self.total_deltas.touched_types.iter(),
            touched_rel_kinds: self.total_deltas.touched_rel_kinds.iter().copied().collect(),
            touched_entities,
        };
        write_commit_summary(&mut self.txn, &self.engine.dbs, summary.clone(), &self.total_deltas.tombstones)?;
        Ok(summary)
    }

    fn commit_with_summary(self, want_summary: bool) -> Result<(CommitReceipt, Option<CommitDeltaSummary>)> {
        let receipt = self.txn.commit()?;
        let summary = want_summary.then(|| CommitDeltaSummary {
            seq: self.commit_seq,
            txnid: receipt.txnid,
            schema_epoch: self.schema.epoch,
            touched_types: self.total_deltas.touched_types,
            touched_rel_kinds: self.total_deltas.touched_rel_kinds.into_iter().collect(),
            entity_deltas: self.total_deltas.entities.into_values().collect(),
            edge_deltas: self.total_deltas.edges.into_values().collect(),
        });
        Ok((receipt, summary))
    }

    fn resolve_target(&self, target: &EntityTarget, slots: &[(EntityKey, String)]) -> Result<EntityKey> {
        match target {
            EntityTarget::BatchSlot(idx) => slots
                .get(*idx as usize)
                .map(|(k, _)| *k)
                .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid BatchSlot({idx})"))),
            EntityTarget::ExternalId(ext) => {
                let bytes = self
                    .txn
                    .get(self.engine.dbs.ext_id, ext.as_bytes())?
                    .ok_or_else(|| ElmdbError::new(ErrorCode::EntityNotFound, format!("entity '{ext}' not found")))?;
                Ok(EntityKey(u64::from_le_bytes(
                    bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "corrupt ext_id entry"))?,
                )))
            }
        }
    }

    fn alloc_entity_key(&mut self) -> Result<EntityKey> {
        let next = match self.txn.get(self.engine.dbs.sequences, SEQ_ENTITY_KEY)? {
            Some(bytes) => decode_u64_le(bytes, "SEQ_ENTITY_KEY")?
                .checked_add(1)
                .ok_or_else(|| ElmdbError::new(ErrorCode::ResourceLimit, "EntityKey u64 exhausted"))?,
            None => 1,
        };
        self.txn.put(self.engine.dbs.sequences, SEQ_ENTITY_KEY, &next.to_le_bytes())?;
        Ok(EntityKey(next))
    }

    fn alloc_rel_key(&mut self) -> Result<RelKey> {
        let next = match self.txn.get(self.engine.dbs.sequences, SEQ_REL_KEY)? {
            Some(bytes) => decode_u64_le(bytes, "SEQ_REL_KEY")?
                .checked_add(1)
                .ok_or_else(|| ElmdbError::new(ErrorCode::ResourceLimit, "RelKey u64 exhausted"))?,
            None => 1,
        };
        self.txn.put(self.engine.dbs.sequences, SEQ_REL_KEY, &next.to_le_bytes())?;
        Ok(RelKey(next))
    }

    fn apply_op(&mut self, op: &WireOp, slots: &mut Vec<(EntityKey, String)>) -> Result<()> {
        match op {
            WireOp::Create { type_name, external_id, fields } => {
                let tdef = match self.schema.type_by_name_any(type_name) {
                    Some(t) if t.archived => {
                        return Err(ElmdbError::new(
                            ErrorCode::TypeArchived,
                            format!("type '{type_name}' is archived and read-only"),
                        ));
                    }
                    Some(t) => t.clone(),
                    None => {
                        return Err(ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{type_name}'")));
                    }
                };

                let key = self.alloc_entity_key()?;
                let ext_id = match external_id {
                    Some(id) => {
                        if id.is_empty() || id.len() > MAX_EXT_ID_BYTES {
                            return Err(ElmdbError::new(
                                ErrorCode::InvalidArgument,
                                format!("external ID length {} out of 1..={MAX_EXT_ID_BYTES}", id.len()),
                            ));
                        }
                        id.clone()
                    }
                    None => format!("{}-{}", tdef.name, key.0),
                };

                if !self.txn.put_no_overwrite(self.engine.dbs.ext_id, ext_id.as_bytes(), &key.0.to_le_bytes())? {
                    return Err(ElmdbError::new(
                        ErrorCode::EntityAlreadyExists,
                        format!("entity '{ext_id}' already exists"),
                    ));
                }

                let mut declared = BTreeMap::new();
                let mut extra = BTreeMap::new();
                let mut changed_fields = FieldMask::empty();

                for (fname, raw_val) in fields {
                    if fname == "__proto__" {
                        return Err(ElmdbError::new(
                            ErrorCode::ValidationFailed,
                            "reserved field name '__proto__' is not permitted",
                        ));
                    }
                    raw_val.validate_canonical()?;
                    if let Some(fdef) = tdef.field_by_name(fname) {
                        let resolved = self.validate_and_resolve_field(fdef, raw_val, slots)?;
                        changed_fields.insert(fdef.id);
                        declared.insert(fdef.id, resolved);
                    } else if tdef.additional_fields {
                        if raw_val.contains_internal_ref() {
                            return Err(ElmdbError::new(
                                ErrorCode::ValidationFailed,
                                format!("extra field '{fname}' cannot contain internal EntityRef/DanglingRef"),
                            ));
                        }
                        extra.insert(fname.clone(), raw_val.clone());
                    } else {
                        return Err(ElmdbError::new(
                            ErrorCode::UnknownField,
                            format!("unknown field '{fname}' on type '{}'", tdef.name),
                        ));
                    }
                }

                // Check all required declared fields are present.
                for fdef in &tdef.fields {
                    if !fdef.archived && !fdef.optional && !declared.contains_key(&fdef.id) {
                        return Err(ElmdbError::new(
                            ErrorCode::ValidationFailed,
                            format!("missing required field '{}.{}'", tdef.name, fdef.name),
                        ));
                    }
                }

                let row = PackedRow {
                    header: RowHeader {
                        type_id: tdef.id,
                        flags: 0,
                        field_count: declared.len() as u16,
                        ext_id: ext_id.clone(),
                        updated_seq: self.commit_seq,
                        created_at_ms: self.now_ms,
                        updated_at_ms: self.now_ms,
                        trashed_at_ms: 0,
                    },
                    fields: declared,
                    extra,
                };

                self.write_row_and_blobs(key, None, &row)?;
                self.txn.put(self.engine.dbs.adj, &type_list_key(tdef.id, key), &[])?;
                self.update_ref_in_for_row(key, None, Some(&row))?;

                self.pending_deltas.record_entity(key, tdef.id, None, Some(row), changed_fields, true);
                slots.push((key, ext_id));
                Ok(())
            }
            WireOp::Patch { target, fields } => {
                let key = self.resolve_target(target, slots)?;
                let old_row = self.load_packed_row(key)?;
                let tdef = self.require_active_type(old_row.header.type_id)?;
                if old_row.header.is_trashed() {
                    return Err(ElmdbError::new(
                        ErrorCode::EntityTrashed,
                        format!("entity '{}' is trashed", old_row.header.ext_id),
                    ));
                }

                let mut next = old_row.clone();
                let mut changed_fields = FieldMask::empty();
                for (fname, raw_val) in fields {
                    if fname == "__proto__" {
                        return Err(ElmdbError::new(
                            ErrorCode::ValidationFailed,
                            "reserved field name '__proto__' is not permitted",
                        ));
                    }
                    raw_val.validate_canonical()?;
                    if let Some(fdef) = tdef.field_by_name(fname) {
                        let resolved = self.validate_and_resolve_field(fdef, raw_val, slots)?;
                        if next.fields.get(&fdef.id) != Some(&resolved) {
                            changed_fields.insert(fdef.id);
                            next.fields.insert(fdef.id, resolved);
                        }
                    } else if tdef.additional_fields {
                        if raw_val.contains_internal_ref() {
                            return Err(ElmdbError::new(
                                ErrorCode::ValidationFailed,
                                format!("extra field '{fname}' cannot contain internal EntityRef/DanglingRef"),
                            ));
                        }
                        next.extra.insert(fname.clone(), raw_val.clone());
                    } else {
                        return Err(ElmdbError::new(
                            ErrorCode::UnknownField,
                            format!("unknown field '{fname}' on type '{}'", tdef.name),
                        ));
                    }
                }
                next.header.updated_seq = self.commit_seq;
                next.header.updated_at_ms = self.now_ms;
                next.header.field_count = next.fields.len() as u16;

                self.write_row_and_blobs(key, Some(&old_row), &next)?;
                self.update_ref_in_for_row(key, Some(&old_row), Some(&next))?;
                self.pending_deltas.record_entity(key, tdef.id, Some(old_row), Some(next), changed_fields, false);
                Ok(())
            }
            WireOp::Unset { target, fields } => {
                let key = self.resolve_target(target, slots)?;
                let old_row = self.load_packed_row(key)?;
                let tdef = self.require_active_type(old_row.header.type_id)?;
                if old_row.header.is_trashed() {
                    return Err(ElmdbError::new(
                        ErrorCode::EntityTrashed,
                        format!("entity '{}' is trashed", old_row.header.ext_id),
                    ));
                }

                let mut next = old_row.clone();
                let mut changed_fields = FieldMask::empty();
                for fname in fields {
                    if let Some(fdef) = tdef.field_by_name(fname) {
                        if !fdef.optional {
                            return Err(ElmdbError::new(
                                ErrorCode::ValidationFailed,
                                format!("cannot unset required field '{}.{}'", tdef.name, fdef.name),
                            ));
                        }
                        if next.fields.remove(&fdef.id).is_some() {
                            changed_fields.insert(fdef.id);
                        }
                    } else if tdef.additional_fields {
                        next.extra.remove(fname);
                    } else {
                        return Err(ElmdbError::new(
                            ErrorCode::UnknownField,
                            format!("unknown field '{fname}' on type '{}'", tdef.name),
                        ));
                    }
                }
                next.header.updated_seq = self.commit_seq;
                next.header.updated_at_ms = self.now_ms;
                next.header.field_count = next.fields.len() as u16;

                self.write_row_and_blobs(key, Some(&old_row), &next)?;
                self.update_ref_in_for_row(key, Some(&old_row), Some(&next))?;
                self.pending_deltas.record_entity(key, tdef.id, Some(old_row), Some(next), changed_fields, false);
                Ok(())
            }
            WireOp::Trash { target } => {
                let key = self.resolve_target(target, slots)?;
                let old_row = self.load_packed_row(key)?;
                let tdef = self.require_active_type(old_row.header.type_id)?;
                if old_row.header.is_trashed() {
                    return Err(ElmdbError::new(
                        ErrorCode::EntityTrashed,
                        format!("entity '{}' is already trashed", old_row.header.ext_id),
                    ));
                }
                let mut next = old_row.clone();
                next.header.flags = make_trashed_flags(true);
                next.header.trashed_at_ms = self.now_ms;
                next.header.updated_seq = self.commit_seq;
                next.header.updated_at_ms = self.now_ms;

                self.write_row_and_blobs(key, Some(&old_row), &next)?;
                self.pending_deltas.record_entity(key, tdef.id, Some(old_row), Some(next), FieldMask::empty(), true);
                Ok(())
            }
            WireOp::Restore { target } => {
                let key = self.resolve_target(target, slots)?;
                let old_row = self.load_packed_row(key)?;
                let tdef = self.require_active_type(old_row.header.type_id)?;
                if !old_row.header.is_trashed() {
                    return Err(ElmdbError::new(
                        ErrorCode::EntityNotTrashed,
                        format!("entity '{}' is not trashed", old_row.header.ext_id),
                    ));
                }
                let mut next = old_row.clone();
                next.header.flags = make_trashed_flags(false);
                next.header.trashed_at_ms = 0;
                next.header.updated_seq = self.commit_seq;
                next.header.updated_at_ms = self.now_ms;

                self.write_row_and_blobs(key, Some(&old_row), &next)?;
                self.pending_deltas.record_entity(key, tdef.id, Some(old_row), Some(next), FieldMask::empty(), true);
                Ok(())
            }
            WireOp::Destroy { target } => {
                let key = self.resolve_target(target, slots)?;
                let old_row = self.load_packed_row(key)?;
                // Archived types remain purgeable via Destroy so uninstalled pack data can be cleaned up.
                let type_id = old_row.header.type_id;

                // 1. Enforce `ref_in` `onDelete` policies (read-only Restrict check first, then SetNull / AllowDangling).
                self.enforce_ref_in_on_destroy(key, &old_row.header.ext_id)?;

                // 2. Cascade-remove all incident edges (both outgoing and incoming).
                let incident_rel_keys = self.collect_incident_rel_keys(key)?;
                for rel_key in incident_rel_keys {
                    self.delete_edge_by_key(rel_key)?;
                }

                // 3. Remove referrer entries owned by this entity, blobs, ext_id, type_list, and row.
                self.update_ref_in_for_row(key, Some(&old_row), None)?;
                if let Some(raw) = self.txn.get(self.engine.dbs.entities, &key.to_ne_bytes())? {
                    let old_blobs = blob_field_ids_of(raw)?;
                    for fid in old_blobs {
                        self.txn.del(self.engine.dbs.blobs, &blob_key(key, fid))?;
                    }
                }
                self.txn.del(self.engine.dbs.ext_id, old_row.header.ext_id.as_bytes())?;
                self.txn.del(self.engine.dbs.adj, &type_list_key(type_id, key))?;
                self.txn.del(self.engine.dbs.entities, &key.to_ne_bytes())?;

                let created_in_txn = self.pending_deltas.entities.get(&key).is_some_and(|e| e.before.is_none())
                    || self.total_deltas.entities.get(&key).is_some_and(|e| e.before.is_none());
                if !created_in_txn {
                    self.pending_deltas.tombstones.push((key, type_id, old_row.header.ext_id.clone()));
                }
                self.pending_deltas.record_entity(key, type_id, Some(old_row), None, FieldMask::empty(), true);
                Ok(())
            }
            WireOp::Link { source, rel_kind, target, weight, info } => {
                let src = self.resolve_target(source, slots)?;
                let dst = self.resolve_target(target, slots)?;
                let rdef = self
                    .schema
                    .relation_by_name(rel_kind)
                    .ok_or_else(|| {
                        ElmdbError::new(ErrorCode::UnknownRelation, format!("unknown relation '{rel_kind}'"))
                    })?
                    .clone();

                let src_hdr = self.load_row_header(src)?;
                let dst_hdr = self.load_row_header(dst)?;
                let _ = self.require_active_type(src_hdr.type_id)?;
                let _ = self.require_active_type(dst_hdr.type_id)?;
                if src_hdr.is_trashed() || dst_hdr.is_trashed() {
                    return Err(ElmdbError::new(ErrorCode::EntityTrashed, "cannot link a trashed entity"));
                }

                if !rdef.endpoints.is_empty() && !rdef.endpoints.contains(&(src_hdr.type_id, dst_hdr.type_id)) {
                    return Err(ElmdbError::new(
                        ErrorCode::EndpointMismatch,
                        format!("relation '{}' does not allow these endpoint types", rdef.name),
                    ));
                }

                // Enforce cardinality and reject duplicate parallel edges on single-valued cardinalities.
                self.check_cardinality(&rdef, src, dst)?;

                // Enforce acyclicity via zero-String-allocation `FxHashSet<u64>` DFS.
                if rdef.acyclic && (src == dst || self.reachable_via(rdef.id, dst, src)?) {
                    return Err(ElmdbError::new(
                        ErrorCode::CycleDetected,
                        format!("linking '{}' would introduce a cycle", rdef.name),
                    ));
                }

                let w = match weight {
                    Some(bits) => {
                        let f = f64::from_bits(*bits);
                        if !f.is_finite() {
                            return Err(ElmdbError::new(ErrorCode::ValidationFailed, "relation weight must be finite"));
                        }
                        if !rdef.weighted && f != 1.0 {
                            return Err(ElmdbError::new(
                                ErrorCode::ValidationFailed,
                                format!("relation '{}' is unweighted", rdef.name),
                            ));
                        }
                        if f == 0.0 { 0.0 } else { f }
                    }
                    None => 1.0,
                };
                if let Some(info_val) = info {
                    info_val.validate_canonical()?;
                    if info_val.contains_internal_ref() {
                        return Err(ElmdbError::new(
                            ErrorCode::ValidationFailed,
                            "relation info cannot contain internal EntityRef/DanglingRef",
                        ));
                    }
                }

                let rel_key = self.alloc_rel_key()?;
                let edge = PackedEdge {
                    key: rel_key,
                    kind: rdef.id,
                    src,
                    dst,
                    weight: w,
                    created_at_ms: self.now_ms,
                    info: info.clone(),
                };
                self.txn.put(self.engine.dbs.relations, &rel_key.to_ne_bytes(), &edge.encode())?;
                self.write_adj_entries(&edge)?;
                self.pending_deltas.record_edge(
                    rel_key,
                    rdef.id,
                    None,
                    Some(edge),
                    (Some(src_hdr.type_id), Some(dst_hdr.type_id)),
                );
                Ok(())
            }
            WireOp::Unlink { source, rel_kind, target } => {
                let src = self.resolve_target(source, slots)?;
                let src_hdr = self.load_row_header(src)?;
                let _ = self.require_active_type(src_hdr.type_id)?;
                let kind_filter = match rel_kind {
                    Some(name) => Some(
                        self.schema
                            .relation_by_name(name)
                            .ok_or_else(|| {
                                ElmdbError::new(ErrorCode::UnknownRelation, format!("unknown relation '{name}'"))
                            })?
                            .id,
                    ),
                    None => None,
                };
                let dst_filter = match target {
                    Some(t) => {
                        let d = self.resolve_target(t, slots)?;
                        let _ = self.load_row_header(d)?;
                        Some(d)
                    }
                    None => None,
                };
                let to_delete = self.find_outgoing_edges(src, kind_filter, dst_filter)?;
                for rel_key in to_delete {
                    self.delete_edge_by_key(rel_key)?;
                }
                Ok(())
            }
        }
    }

    fn require_active_type(&self, type_id: TypeId) -> Result<crate::catalog::TypeDef> {
        let tdef = self
            .schema
            .type_by_id(type_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, format!("missing TypeDef {type_id:?}")))?;
        if tdef.archived {
            return Err(ElmdbError::new(
                ErrorCode::TypeArchived,
                format!("type '{}' is archived and read-only", tdef.name),
            ));
        }
        Ok(tdef.clone())
    }

    fn load_row_header(&self, key: EntityKey) -> Result<RowHeader> {
        let raw = self
            .txn
            .get(self.engine.dbs.entities, &key.to_ne_bytes())?
            .ok_or_else(|| ElmdbError::new(ErrorCode::EntityNotFound, format!("entity key {} not found", key.0)))?;
        Ok(decode_row_header(raw)?.0)
    }

    fn load_packed_row(&self, key: EntityKey) -> Result<PackedRow> {
        let raw = self
            .txn
            .get(self.engine.dbs.entities, &key.to_ne_bytes())?
            .ok_or_else(|| ElmdbError::new(ErrorCode::EntityNotFound, format!("entity key {} not found", key.0)))?
            .to_vec();
        decode_row(&raw, |fid| Ok(self.txn.get(self.engine.dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned)))
    }

    fn write_row_and_blobs(&mut self, key: EntityKey, old: Option<&PackedRow>, next: &PackedRow) -> Result<()> {
        let prev_blobs: FxHashSet<FieldId> = if old.is_some() {
            match self.txn.get(self.engine.dbs.entities, &key.to_ne_bytes())? {
                Some(raw) => blob_field_ids_of(raw)?.into_iter().collect(),
                None => FxHashSet::default(),
            }
        } else {
            FxHashSet::default()
        };

        let enc = encode_row(next)?;
        let new_blob_set: FxHashSet<FieldId> = enc.blob_field_ids.iter().copied().collect();

        // Only rewrite blobs whose value actually changed or newly spilled!
        for (fid, blob_bytes) in enc.blobs {
            let unchanged = old.is_some_and(|o| {
                prev_blobs.contains(&fid)
                    && if fid == crate::row::EXTRA_BLOB_FIELD_ID {
                        o.extra == next.extra
                    } else {
                        o.fields.get(&fid) == next.fields.get(&fid)
                    }
            });
            if !unchanged {
                self.txn.put(self.engine.dbs.blobs, &blob_key(key, fid), &blob_bytes)?;
            }
        }
        // Delete any previously spilled blobs that are no longer spilled or were unset.
        for old_fid in prev_blobs {
            if !new_blob_set.contains(&old_fid) {
                self.txn.del(self.engine.dbs.blobs, &blob_key(key, old_fid))?;
            }
        }
        self.txn.put(self.engine.dbs.entities, &key.to_ne_bytes(), &enc.row_bytes)?;
        Ok(())
    }

    fn validate_and_resolve_field(
        &self,
        fdef: &FieldDef,
        val: &CanonicalValue,
        slots: &[(EntityKey, String)],
    ) -> Result<CanonicalValue> {
        if matches!(val, CanonicalValue::Null) {
            if fdef.nullable {
                return Ok(CanonicalValue::Null);
            }
            return Err(ElmdbError::new(ErrorCode::ValidationFailed, format!("field '{}' is not nullable", fdef.name)));
        }
        self.validate_kind(&fdef.kind, val, &fdef.name, slots)
    }

    fn validate_kind(
        &self,
        kind: &FieldKind,
        val: &CanonicalValue,
        path: &str,
        slots: &[(EntityKey, String)],
    ) -> Result<CanonicalValue> {
        match (kind, val) {
            (FieldKind::String, CanonicalValue::String(_))
            | (FieldKind::Boolean, CanonicalValue::Bool(_))
            | (FieldKind::Bytes, CanonicalValue::Bytes(_))
            | (FieldKind::Timestamp, CanonicalValue::Timestamp { .. }) => Ok(val.clone()),
            (FieldKind::Object, CanonicalValue::Object(_)) | (FieldKind::Any, _) => {
                if val.contains_internal_ref() {
                    return Err(ElmdbError::new(
                        ErrorCode::ValidationFailed,
                        format!("field '{path}' cannot contain internal EntityRef/DanglingRef"),
                    ));
                }
                Ok(val.clone())
            }
            (FieldKind::Timestamp, CanonicalValue::Int(ms)) => Ok(CanonicalValue::Timestamp { ms: *ms, ns: 0 }),
            (FieldKind::Number, CanonicalValue::Int(i)) => Ok(CanonicalValue::Int(*i)),
            (FieldKind::Number, CanonicalValue::Float(f)) => CanonicalValue::float(*f),
            (FieldKind::Int64, CanonicalValue::Int(_)) => Ok(val.clone()),
            (FieldKind::Integer, CanonicalValue::Int(i))
                if (-9_007_199_254_740_991..=9_007_199_254_740_991).contains(i) =>
            {
                Ok(CanonicalValue::Int(*i))
            }
            (FieldKind::Integer, CanonicalValue::Float(f))
                if f.fract() == 0.0 && (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(f) =>
            {
                Ok(CanonicalValue::Int(*f as i64))
            }
            (FieldKind::Enum(allowed), CanonicalValue::String(s)) if allowed.iter().any(|a| a == s) => {
                Ok(CanonicalValue::String(s.clone()))
            }
            (FieldKind::Ref { to, .. }, CanonicalValue::String(ext_id)) => {
                let target_key = self.resolve_target(&EntityTarget::ExternalId(ext_id.clone()), slots)?;
                let hdr = self.load_row_header(target_key)?;
                if !to.contains(&hdr.type_id) {
                    return Err(ElmdbError::new(
                        ErrorCode::ValidationFailed,
                        format!("field '{path}' cannot reference entity '{ext_id}' of disallowed type"),
                    ));
                }
                Ok(CanonicalValue::EntityRef(target_key.0))
            }
            (FieldKind::Ref { to, .. }, CanonicalValue::EntityRef(raw_key)) => {
                let hdr = self.load_row_header(EntityKey(*raw_key))?;
                if !to.contains(&hdr.type_id) {
                    return Err(ElmdbError::new(
                        ErrorCode::ValidationFailed,
                        format!("field '{path}' cannot reference entity key {raw_key} of disallowed type"),
                    ));
                }
                Ok(CanonicalValue::EntityRef(*raw_key))
            }
            (FieldKind::Ref { on_delete: OnDeletePolicy::AllowDangling, .. }, CanonicalValue::DanglingRef(ext)) => {
                Ok(CanonicalValue::DanglingRef(ext.clone()))
            }
            (FieldKind::List(elem_kind), CanonicalValue::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.validate_kind(elem_kind, item, path, slots)?);
                }
                Ok(CanonicalValue::Array(out))
            }
            _ => Err(ElmdbError::new(
                ErrorCode::ValidationFailed,
                format!("value does not match declared field type for '{path}'"),
            )),
        }
    }

    fn update_ref_in_for_row(
        &mut self,
        referrer: EntityKey,
        before: Option<&PackedRow>,
        after: Option<&PackedRow>,
    ) -> Result<()> {
        let mut old_refs: FxHashSet<(EntityKey, FieldId)> = FxHashSet::default();
        if let Some(row) = before {
            for (&fid, val) in &row.fields {
                collect_entity_refs(val, fid, &mut old_refs);
            }
        }
        let mut new_refs: FxHashSet<(EntityKey, FieldId)> = FxHashSet::default();
        if let Some(row) = after {
            for (&fid, val) in &row.fields {
                collect_entity_refs(val, fid, &mut new_refs);
            }
        }
        for &(target, fid) in old_refs.difference(&new_refs) {
            self.txn.del(self.engine.dbs.adj, &ref_in_key(target, referrer, fid))?;
        }
        for &(target, fid) in new_refs.difference(&old_refs) {
            self.txn.put(self.engine.dbs.adj, &ref_in_key(target, referrer, fid), &[])?;
        }
        Ok(())
    }

    fn enforce_ref_in_on_destroy(&mut self, target: EntityKey, target_ext_id: &str) -> Result<()> {
        let mut prefix = [0u8; 9];
        prefix[0] = TAG_REF_IN;
        prefix[1..9].copy_from_slice(&target.to_be_bytes());

        // Collect referrers first before mutating LMDB.
        let mut refs: Vec<(EntityKey, FieldId)> = Vec::new();
        {
            let mut cur = self.txn.cursor(self.engine.dbs.adj)?;
            let mut entry = cur.seek_ge(&prefix)?;
            while let Some((k, _)) = entry {
                if k.len() != 19 || k[0..9] != prefix {
                    break;
                }
                let referrer = EntityKey::from_be_bytes(k[9..17].try_into().expect("8"));
                let fid = FieldId(u16::from_be_bytes([k[17], k[18]]));
                refs.push((referrer, fid));
                entry = cur.next()?;
            }
        }
        if refs.is_empty() {
            return Ok(());
        }

        // Group by referrer so each referrer row is checked & rewritten at most once.
        let mut by_referrer: BTreeMap<EntityKey, Vec<FieldId>> = BTreeMap::new();
        for (r, fid) in refs {
            if r != target {
                by_referrer.entry(r).or_default().push(fid);
            }
        }

        struct RefCleanupPlan {
            referrer_key: EntityKey,
            old_row: PackedRow,
            tdef: crate::catalog::TypeDef,
            field_policies: Vec<(FieldId, OnDeletePolicy)>,
        }

        // Pass 1 (read-only): reject immediately if ANY active Restrict referrer exists before mutating anything!
        let mut plans: Vec<RefCleanupPlan> = Vec::with_capacity(by_referrer.len());
        for (referrer_key, fids) in by_referrer {
            let old_row = self.load_packed_row(referrer_key)?;
            let tdef = self
                .schema
                .type_by_id(old_row.header.type_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing TypeDef for referrer"))?
                .clone();

            let mut field_policies = Vec::with_capacity(fids.len());
            for fid in fids {
                let fdef = tdef
                    .field_by_id(fid)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing FieldDef for ref_in"))?;
                // Archived types/fields fall back to AllowDangling so archived data never permanently locks active rows.
                let policy = if tdef.archived || fdef.archived {
                    OnDeletePolicy::AllowDangling
                } else {
                    fdef.kind.ref_policy().map_or(OnDeletePolicy::Restrict, |(_, p)| p)
                };
                if policy == OnDeletePolicy::Restrict {
                    return Err(ElmdbError::new(
                        ErrorCode::Referenced,
                        format!(
                            "cannot destroy '{}': referenced by '{}' field '{}.{}'",
                            target_ext_id, old_row.header.ext_id, tdef.name, fdef.name
                        ),
                    ));
                }
                field_policies.push((fid, policy));
            }
            plans.push(RefCleanupPlan { referrer_key, old_row, tdef, field_policies });
        }

        // Pass 2: apply SetNull (recursively through nested lists) and AllowDangling rewrites.
        for RefCleanupPlan { referrer_key, old_row, tdef, field_policies } in plans {
            let mut next = old_row.clone();
            let mut changed_fields = FieldMask::empty();

            for (fid, policy) in field_policies {
                let fdef = tdef.field_by_id(fid).expect("validated in pass 1");
                match policy {
                    OnDeletePolicy::Restrict => unreachable!("checked in pass 1"),
                    OnDeletePolicy::SetNull => match next.fields.get_mut(&fid) {
                        Some(CanonicalValue::EntityRef(k)) if *k == target.0 => {
                            if fdef.nullable {
                                next.fields.insert(fid, CanonicalValue::Null);
                            } else {
                                next.fields.remove(&fid);
                            }
                            changed_fields.insert(fid);
                        }
                        Some(val @ CanonicalValue::Array(_)) => {
                            if remove_ref_from_value(val, target.0) {
                                changed_fields.insert(fid);
                            }
                        }
                        _ => {}
                    },
                    OnDeletePolicy::AllowDangling => {
                        if let Some(val) = next.fields.get_mut(&fid)
                            && rewrite_ref_to_dangling(val, target.0, target_ext_id)
                        {
                            changed_fields.insert(fid);
                        }
                    }
                }
            }

            if !changed_fields.is_empty() {
                next.header.updated_seq = self.commit_seq;
                next.header.updated_at_ms = self.now_ms;
                next.header.field_count = next.fields.len() as u16;
                self.write_row_and_blobs(referrer_key, Some(&old_row), &next)?;
                self.update_ref_in_for_row(referrer_key, Some(&old_row), Some(&next))?;
                self.pending_deltas.record_entity(
                    referrer_key,
                    tdef.id,
                    Some(old_row),
                    Some(next),
                    changed_fields,
                    false,
                );
            }
        }
        Ok(())
    }

    fn write_adj_entries(&mut self, edge: &PackedEdge) -> Result<()> {
        let mut val = [0u8; 16];
        val[0..8].copy_from_slice(&edge.dst.to_be_bytes());
        val[8..16].copy_from_slice(&edge.weight.to_bits().to_le_bytes());
        self.txn.put(self.engine.dbs.adj, &adj_key(TAG_ADJ_OUT, edge.src, edge.kind, edge.key), &val)?;

        val[0..8].copy_from_slice(&edge.src.to_be_bytes());
        self.txn.put(self.engine.dbs.adj, &adj_key(TAG_ADJ_IN, edge.dst, edge.kind, edge.key), &val)?;

        self.txn.put(self.engine.dbs.adj, &adj_by_kind_key(edge.kind, edge.key), &[])?;
        Ok(())
    }

    fn delete_edge_by_key(&mut self, rel_key: RelKey) -> Result<()> {
        let Some(raw) = self.txn.get(self.engine.dbs.relations, &rel_key.to_ne_bytes())? else {
            return Ok(());
        };
        let edge = PackedEdge::decode(rel_key, raw)?;
        self.txn.del(self.engine.dbs.adj, &adj_key(TAG_ADJ_OUT, edge.src, edge.kind, rel_key))?;
        self.txn.del(self.engine.dbs.adj, &adj_key(TAG_ADJ_IN, edge.dst, edge.kind, rel_key))?;
        self.txn.del(self.engine.dbs.adj, &adj_by_kind_key(edge.kind, rel_key))?;
        self.txn.del(self.engine.dbs.relations, &rel_key.to_ne_bytes())?;

        // Track endpoint types so `beforeCommit` hooks watching those types see the unlink!
        let src_ty = self.load_row_header(edge.src).ok().map(|h| h.type_id);
        let dst_ty = self.load_row_header(edge.dst).ok().map(|h| h.type_id);
        self.pending_deltas.record_edge(rel_key, edge.kind, Some(edge), None, (src_ty, dst_ty));
        Ok(())
    }

    fn check_cardinality(&self, rdef: &crate::catalog::RelKindDef, src: EntityKey, dst: EntityKey) -> Result<()> {
        match rdef.cardinality {
            Cardinality::ManyToMany => Ok(()),
            Cardinality::ManyToOne => {
                if let Some(existing) = self.first_adj_far(TAG_ADJ_OUT, src, rdef.id)? {
                    if existing == dst {
                        return Err(ElmdbError::new(
                            ErrorCode::RelationAlreadyExists,
                            format!("relation '{}' already links these entities", rdef.name),
                        ));
                    }
                    return Err(ElmdbError::new(
                        ErrorCode::CardinalityViolation,
                        format!("relation '{}' is many-to-one (source already linked elsewhere)", rdef.name),
                    ));
                }
                Ok(())
            }
            Cardinality::OneToMany => {
                if let Some(existing) = self.first_adj_far(TAG_ADJ_IN, dst, rdef.id)? {
                    if existing == src {
                        return Err(ElmdbError::new(
                            ErrorCode::RelationAlreadyExists,
                            format!("relation '{}' already links these entities", rdef.name),
                        ));
                    }
                    return Err(ElmdbError::new(
                        ErrorCode::CardinalityViolation,
                        format!("relation '{}' is one-to-many (target already linked from elsewhere)", rdef.name),
                    ));
                }
                Ok(())
            }
            Cardinality::OneToOne => {
                if let Some(existing) = self.first_adj_far(TAG_ADJ_OUT, src, rdef.id)? {
                    if existing == dst {
                        return Err(ElmdbError::new(
                            ErrorCode::RelationAlreadyExists,
                            format!("relation '{}' already links these entities", rdef.name),
                        ));
                    }
                    return Err(ElmdbError::new(
                        ErrorCode::CardinalityViolation,
                        format!("relation '{}' is one-to-one", rdef.name),
                    ));
                }
                if self.first_adj_far(TAG_ADJ_IN, dst, rdef.id)?.is_some() {
                    return Err(ElmdbError::new(
                        ErrorCode::CardinalityViolation,
                        format!("relation '{}' is one-to-one", rdef.name),
                    ));
                }
                Ok(())
            }
        }
    }

    fn first_adj_far(&self, tag: u8, node: EntityKey, kind: RelKindId) -> Result<Option<EntityKey>> {
        let mut prefix = [0u8; 11];
        prefix[0] = tag;
        prefix[1..9].copy_from_slice(&node.to_be_bytes());
        prefix[9..11].copy_from_slice(&kind.0.to_be_bytes());

        let mut cur = self.txn.cursor(self.engine.dbs.adj)?;
        if let Some((k, v)) = cur.seek_ge(&prefix)?
            && k.len() == 19
            && k[0..11] == prefix
        {
            if v.len() < 8 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated adj value"));
            }
            return Ok(Some(EntityKey(decode_u64_be(&v[0..8], "adj far")?)));
        }
        Ok(None)
    }

    fn reachable_via(&self, kind: RelKindId, start: EntityKey, goal: EntityKey) -> Result<bool> {
        let mut visited: FxHashSet<u64> = FxHashSet::default();
        let mut stack = vec![start];
        visited.insert(start.0);

        while let Some(node) = stack.pop() {
            let mut prefix = [0u8; 11];
            prefix[0] = TAG_ADJ_OUT;
            prefix[1..9].copy_from_slice(&node.to_be_bytes());
            prefix[9..11].copy_from_slice(&kind.0.to_be_bytes());

            let mut cur = self.txn.cursor(self.engine.dbs.adj)?;
            let mut entry = cur.seek_ge(&prefix)?;
            while let Some((k, v)) = entry {
                if k.len() != 19 || k[0..11] != prefix {
                    break;
                }
                let far = EntityKey::from_be_bytes(v[0..8].try_into().expect("8"));
                if far == goal {
                    return Ok(true);
                }
                if visited.insert(far.0) {
                    stack.push(far);
                }
                entry = cur.next()?;
            }
        }
        Ok(false)
    }

    fn find_outgoing_edges(
        &self,
        src: EntityKey,
        kind_filter: Option<RelKindId>,
        dst_filter: Option<EntityKey>,
    ) -> Result<Vec<RelKey>> {
        let mut prefix = Vec::with_capacity(11);
        prefix.push(TAG_ADJ_OUT);
        prefix.extend_from_slice(&src.to_be_bytes());
        if let Some(k) = kind_filter {
            prefix.extend_from_slice(&k.0.to_be_bytes());
        }
        let mut out = Vec::new();
        let mut cur = self.txn.cursor(self.engine.dbs.adj)?;
        let mut entry = cur.seek_ge(&prefix)?;
        while let Some((k, v)) = entry {
            if !k.starts_with(&prefix) || k.len() != 19 {
                break;
            }
            let far = EntityKey::from_be_bytes(v[0..8].try_into().expect("8"));
            if dst_filter.is_none_or(|d| d == far) {
                out.push(RelKey::from_be_bytes(k[11..19].try_into().expect("8")));
            }
            entry = cur.next()?;
        }
        Ok(out)
    }

    fn collect_incident_rel_keys(&self, node: EntityKey) -> Result<Vec<RelKey>> {
        let mut out = BTreeSet::new();
        for tag in [TAG_ADJ_OUT, TAG_ADJ_IN] {
            let mut prefix = [0u8; 9];
            prefix[0] = tag;
            prefix[1..9].copy_from_slice(&node.to_be_bytes());
            let mut cur = self.txn.cursor(self.engine.dbs.adj)?;
            let mut entry = cur.seek_ge(&prefix)?;
            while let Some((k, _)) = entry {
                if k.len() != 19 || k[0..9] != prefix {
                    break;
                }
                out.insert(RelKey::from_be_bytes(k[11..19].try_into().expect("8")));
                entry = cur.next()?;
            }
        }
        Ok(out.into_iter().collect())
    }
}

// ---------------------------------------------------------------------------
// Key builders & read helpers
// ---------------------------------------------------------------------------

fn validate_counter_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_COUNTER_NAME_BYTES {
        return Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("counter name length {} out of 1..={MAX_COUNTER_NAME_BYTES}", name.len()),
        ));
    }
    if name.as_bytes().contains(&0) {
        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "counter name must not contain NUL bytes"));
    }
    Ok(())
}

fn counter_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + name.len());
    key.push(SEQ_COUNTER_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

/// Atomically increments named transactional counter `name` inside `txn` and returns the new value (`1, 2, ...`).
pub fn next_counter_in_txn<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    dbs: &SystemDbs,
    name: &str,
) -> Result<u64> {
    validate_counter_name(name)?;
    let key = counter_key(name);
    let next = match txn.get(dbs.sequences, &key)? {
        Some(bytes) => decode_u64_le(bytes, "counter")?
            .checked_add(1)
            .ok_or_else(|| ElmdbError::new(ErrorCode::ResourceLimit, format!("counter '{name}' u64 exhausted")))?,
        None => 1,
    };
    txn.put(dbs.sequences, &key, &next.to_le_bytes())?;
    Ok(next)
}

/// Reads the current value of named transactional counter `name` (`0` if unset).
pub fn get_counter_in_txn<R: KvRead>(txn: &R, dbs: &SystemDbs, name: &str) -> Result<u64> {
    validate_counter_name(name)?;
    let key = counter_key(name);
    match txn.get(dbs.sequences, &key)? {
        Some(bytes) => decode_u64_le(bytes, "counter"),
        None => Ok(0),
    }
}

/// Sets named transactional counter `name` to `value` inside `txn`.
pub fn set_counter_in_txn<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    dbs: &SystemDbs,
    name: &str,
    value: u64,
) -> Result<()> {
    validate_counter_name(name)?;
    let key = counter_key(name);
    txn.put(dbs.sequences, &key, &value.to_le_bytes())?;
    Ok(())
}

/// Lists all persisted named transactional counters (used by lossless `dump()`).
pub fn list_counters_in_txn<R: KvRead>(txn: &R, dbs: &SystemDbs) -> Result<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    let mut cur = txn.cursor(dbs.sequences)?;
    let prefix = [SEQ_COUNTER_PREFIX];
    let mut entry = cur.seek_ge(&prefix)?;
    while let Some((k, v)) = entry {
        if k.first() != Some(&SEQ_COUNTER_PREFIX) {
            break;
        }
        let name = std::str::from_utf8(&k[1..])
            .map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "non-UTF-8 counter name in sequences"))?;
        let val = decode_u64_le(v, "counter")?;
        out.insert(name.to_owned(), val);
        entry = cur.next()?;
    }
    Ok(out)
}

fn type_list_key(type_id: TypeId, entity: EntityKey) -> [u8; 11] {
    let mut k = [0u8; 11];
    k[0] = TAG_TYPE_LIST;
    k[1..3].copy_from_slice(&type_id.0.to_be_bytes());
    k[3..11].copy_from_slice(&entity.to_be_bytes());
    k
}

fn ref_in_key(target: EntityKey, referrer: EntityKey, fid: FieldId) -> [u8; 19] {
    let mut k = [0u8; 19];
    k[0] = TAG_REF_IN;
    k[1..9].copy_from_slice(&target.to_be_bytes());
    k[9..17].copy_from_slice(&referrer.to_be_bytes());
    k[17..19].copy_from_slice(&fid.0.to_be_bytes());
    k
}

fn adj_key(tag: u8, node: EntityKey, kind: RelKindId, rel_key: RelKey) -> [u8; 19] {
    let mut k = [0u8; 19];
    k[0] = tag;
    k[1..9].copy_from_slice(&node.to_be_bytes());
    k[9..11].copy_from_slice(&kind.0.to_be_bytes());
    k[11..19].copy_from_slice(&rel_key.to_be_bytes());
    k
}

fn adj_by_kind_key(kind: RelKindId, rel_key: RelKey) -> [u8; 11] {
    let mut k = [0u8; 11];
    k[0] = TAG_ADJ_BY_KIND;
    k[1..3].copy_from_slice(&kind.0.to_be_bytes());
    k[3..11].copy_from_slice(&rel_key.to_be_bytes());
    k
}

fn collect_entity_refs(val: &CanonicalValue, fid: FieldId, out: &mut FxHashSet<(EntityKey, FieldId)>) {
    match val {
        CanonicalValue::EntityRef(k) => {
            out.insert((EntityKey(*k), fid));
        }
        CanonicalValue::Array(items) => {
            for item in items {
                collect_entity_refs(item, fid, out);
            }
        }
        _ => {}
    }
}

fn rewrite_ref_to_dangling(val: &mut CanonicalValue, target: u64, ext_id: &str) -> bool {
    match val {
        CanonicalValue::EntityRef(k) if *k == target => {
            *val = CanonicalValue::DanglingRef(ext_id.to_owned());
            true
        }
        CanonicalValue::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= rewrite_ref_to_dangling(item, target, ext_id);
            }
            changed
        }
        _ => false,
    }
}

fn remove_ref_from_value(val: &mut CanonicalValue, target: u64) -> bool {
    match val {
        CanonicalValue::Array(items) => {
            let before = items.len();
            items.retain(|item| !matches!(item, CanonicalValue::EntityRef(k) if *k == target));
            let mut changed = items.len() != before;
            for item in items.iter_mut() {
                changed |= remove_ref_from_value(item, target);
            }
            changed
        }
        _ => false,
    }
}

fn decode_u64_le(bytes: &[u8], label: &str) -> Result<u64> {
    let arr: [u8; 8] =
        bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt u64 LE for {label}")))?;
    Ok(u64::from_le_bytes(arr))
}

fn decode_u64_be(bytes: &[u8], label: &str) -> Result<u64> {
    let arr: [u8; 8] =
        bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt u64 BE for {label}")))?;
    Ok(u64::from_be_bytes(arr))
}

fn try_open_system_dbs_ro<S: KvStore, V: Vfs>(
    gate: &DurabilityGate<S, V>,
    ro: &S::RoTxn<'_>,
) -> Result<Option<SystemDbs>> {
    let (
        Some(meta),
        Some(ext_id),
        Some(entities),
        Some(blobs),
        Some(relations),
        Some(rel_blobs),
        Some(embeddings),
        Some(sequences),
        Some(commit_log),
        Some(adj),
    ) = (
        gate.open_subdb_ro(ro, "meta", DbFlags::DEFAULT)?,
        gate.open_subdb_ro(ro, "ext_id", DbFlags::DEFAULT)?,
        gate.open_subdb_ro(ro, "entities", DbFlags::INTEGER_KEY)?,
        gate.open_subdb_ro(ro, "blobs", DbFlags::DEFAULT)?,
        gate.open_subdb_ro(ro, "relations", DbFlags::INTEGER_KEY)?,
        gate.open_subdb_ro(ro, "rel_blobs", DbFlags::DEFAULT)?,
        gate.open_subdb_ro(ro, "embeddings", DbFlags::DEFAULT)?,
        gate.open_subdb_ro(ro, "sequences", DbFlags::DEFAULT)?,
        gate.open_subdb_ro(ro, "commit_log", DbFlags::DEFAULT)?,
        gate.open_subdb_ro(ro, "adj", DbFlags::DEFAULT)?,
    )
    else {
        return Ok(None);
    };
    Ok(Some(SystemDbs { meta, ext_id, entities, blobs, relations, rel_blobs, embeddings, sequences, commit_log, adj }))
}

fn load_schema_and_pool<R: KvRead>(txn: &R, dbs: &SystemDbs) -> Result<(CompiledSchema, SubDbPool)> {
    let bytes = txn
        .get(dbs.meta, META_SCHEMA_KEY)?
        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing meta schema"))?;
    let schema: CompiledSchema = serde_json::from_slice(bytes)
        .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt meta schema: {e}")))?;
    let pool_bytes = txn
        .get(dbs.meta, META_POOL_KEY)?
        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing meta subdb_pool"))?;
    let pool = SubDbPool::decode(pool_bytes)?;
    Ok((schema, pool))
}

fn read_single_field<R: KvRead>(
    txn: &R,
    schema: &CompiledSchema,
    dbs: &SystemDbs,
    ext_id: &str,
    field_name: &str,
) -> Result<Option<CanonicalValue>> {
    let Some(key_bytes) = txn.get(dbs.ext_id, ext_id.as_bytes())? else {
        return Ok(None);
    };
    let key = EntityKey(decode_u64_le(key_bytes, "ext_id")?);
    let Some(raw) = txn.get(dbs.entities, &key.to_ne_bytes())? else {
        return Ok(None);
    };
    let (hdr, _) = decode_row_header(raw)?;
    let tdef = schema
        .type_by_id(hdr.type_id)
        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "row references unknown TypeId"))?;
    let mut ref_cache = FxHashMap::default();
    if let Some(fdef) = tdef.field_by_name(field_name) {
        let Some(val) = decode_single_field(raw, fdef.id, |fid| {
            Ok(txn.get(dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned))
        })?
        else {
            return Ok(None);
        };
        return Ok(Some(resolve_refs_to_strings(txn, dbs, val, &mut ref_cache)?));
    }
    if !hdr.has_extra() {
        return Ok(None);
    }
    let full = decode_row(raw, |fid| Ok(txn.get(dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned)))?;
    Ok(full.extra.get(field_name).cloned())
}

/// Looks up an entity's internal `EntityKey` by external ID within an active transaction.
pub fn lookup_ext_id_in_txn<R: KvRead>(txn: &R, dbs: &SystemDbs, ext_id: &str) -> Result<Option<EntityKey>> {
    let Some(key_bytes) = txn.get(dbs.ext_id, ext_id.as_bytes())? else {
        return Ok(None);
    };
    Ok(Some(EntityKey(decode_u64_le(key_bytes, "ext_id")?)))
}

/// Lists outgoing relations for `ext_id` within an active read/write transaction.
pub fn read_relations_out_in_txn<R: KvRead>(
    txn: &R,
    schema: &CompiledSchema,
    dbs: &SystemDbs,
    ext_id: &str,
    rel_kind: Option<&str>,
) -> Result<Vec<StoredEdge>> {
    read_relations_for_ext_id(txn, schema, dbs, TAG_ADJ_OUT, ext_id, rel_kind)
}

/// Lists incoming relations for `ext_id` within an active read/write transaction.
pub fn read_relations_in_in_txn<R: KvRead>(
    txn: &R,
    schema: &CompiledSchema,
    dbs: &SystemDbs,
    ext_id: &str,
    rel_kind: Option<&str>,
) -> Result<Vec<StoredEdge>> {
    read_relations_for_ext_id(txn, schema, dbs, TAG_ADJ_IN, ext_id, rel_kind)
}

fn read_relations_for_ext_id<R: KvRead>(
    txn: &R,
    schema: &CompiledSchema,
    dbs: &SystemDbs,
    tag: u8,
    ext_id: &str,
    rel_kind: Option<&str>,
) -> Result<Vec<StoredEdge>> {
    let Some(key_bytes) = txn.get(dbs.ext_id, ext_id.as_bytes())? else {
        return Ok(Vec::new());
    };
    let node = EntityKey(decode_u64_le(key_bytes, "ext_id")?);
    let kind_filter = match rel_kind {
        Some(name) => Some(
            schema
                .relation_by_name(name)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownRelation, format!("unknown relation '{name}'")))?
                .id,
        ),
        None => None,
    };
    read_adj_edges(txn, schema, dbs, tag, node, kind_filter)
}

pub fn read_commit_seq<R: KvRead>(txn: &R, dbs: &SystemDbs) -> Result<u64> {
    match txn.get(dbs.meta, META_COMMIT_SEQ_KEY)? {
        Some(bytes) => Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "corrupt commit_seq"))?,
        )),
        None => Ok(0),
    }
}

pub fn read_schema_epoch<R: KvRead>(txn: &R, dbs: &SystemDbs) -> Result<u64> {
    match txn.get(dbs.meta, META_SCHEMA_EPOCH_KEY)? {
        Some(bytes) => Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "corrupt schema_epoch"))?,
        )),
        None => Ok(0),
    }
}

/// Reads `(tip_seq, summaries_since)` atomically within ONE single MVCC `RoTxn` snapshot (zero TOCTOU race!).
pub fn read_commit_summaries_in_txn<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    since_seq: u64,
) -> Result<(u64, Option<Vec<CrossProcCommitSummary>>)> {
    let tip = read_commit_seq(txn, dbs)?;
    if tip <= since_seq {
        return Ok((tip, Some(Vec::new())));
    }
    if tip.saturating_sub(since_seq) > COMMIT_LOG_WINDOW {
        return Ok((tip, None));
    }
    let mut start_key = [0u8; 9];
    start_key[0] = TAG_COMMIT_SUMMARY;
    start_key[1..9].copy_from_slice(&(since_seq.saturating_add(1)).to_be_bytes());

    let mut cur = txn.cursor(dbs.commit_log)?;
    let mut out = Vec::new();
    let mut entry = cur.seek_ge(&start_key)?;
    while let Some((k, v)) = entry {
        if k.first() != Some(&TAG_COMMIT_SUMMARY) {
            break;
        }
        let summary: CrossProcCommitSummary = serde_json::from_slice(v)
            .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt commit_log summary: {e}")))?;
        out.push(summary);
        entry = cur.next()?;
    }
    Ok((tip, Some(out)))
}

fn persist_schema_and_pool<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    dbs: &SystemDbs,
    schema: &CompiledSchema,
    pool: &SubDbPool,
    commit_seq: u64,
) -> Result<()> {
    let bytes = serde_json::to_vec(schema).map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;
    txn.put(dbs.meta, META_SCHEMA_KEY, &bytes)?;
    txn.put(dbs.meta, META_SCHEMA_EPOCH_KEY, &schema.epoch.to_le_bytes())?;
    txn.put(dbs.meta, META_POOL_KEY, &pool.encode())?;
    txn.put(dbs.meta, META_COMMIT_SEQ_KEY, &commit_seq.to_le_bytes())?;
    Ok(())
}

fn write_commit_summary<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    dbs: &SystemDbs,
    summary: CrossProcCommitSummary,
    tombstones: &[(EntityKey, TypeId, String)],
) -> Result<()> {
    let mut sum_key = [0u8; 9];
    sum_key[0] = TAG_COMMIT_SUMMARY;
    sum_key[1..9].copy_from_slice(&summary.seq.to_be_bytes());
    let encoded = serde_json::to_vec(&summary).map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;
    txn.put(dbs.commit_log, &sum_key, &encoded)?;

    for (key, tid, ext_id) in tombstones {
        let mut tk = [0u8; 17];
        tk[0] = TAG_TOMBSTONE;
        tk[1..9].copy_from_slice(&summary.seq.to_be_bytes());
        tk[9..17].copy_from_slice(&key.to_be_bytes());
        let mut tv = Vec::with_capacity(2 + ext_id.len());
        tv.extend_from_slice(&tid.0.to_le_bytes());
        tv.extend_from_slice(ext_id.as_bytes());
        txn.put(dbs.commit_log, &tk, &tv)?;
    }

    // Prune entries older than COMMIT_LOG_WINDOW.
    if summary.seq > COMMIT_LOG_WINDOW {
        let cutoff = summary.seq - COMMIT_LOG_WINDOW;
        for tag in [TAG_COMMIT_SUMMARY, TAG_TOMBSTONE] {
            let mut to_del = Vec::new();
            {
                let mut cur = txn.cursor(dbs.commit_log)?;
                let mut entry = cur.seek_ge(&[tag])?;
                while let Some((k, _)) = entry {
                    if k.first() != Some(&tag) || k.len() < 9 {
                        break;
                    }
                    let s = u64::from_be_bytes(k[1..9].try_into().expect("8"));
                    if s >= cutoff {
                        break;
                    }
                    to_del.push(k.to_vec());
                    entry = cur.next()?;
                }
            }
            for k in to_del {
                txn.del(dbs.commit_log, &k)?;
            }
        }
    }
    Ok(())
}

pub fn scan_type_keys<R: KvRead>(txn: &R, dbs: &SystemDbs, type_id: TypeId) -> Result<Vec<EntityKey>> {
    scan_type_keys_in_range(txn, dbs, type_id, None, None, usize::MAX)
}

/// Scans `EntityKey`s of `type_id` strictly greater than `after_exclusive` and `<= max_inclusive`,
/// returning at most `limit` keys in ascending order.
pub fn scan_type_keys_in_range<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    type_id: TypeId,
    after_exclusive: Option<EntityKey>,
    max_inclusive: Option<EntityKey>,
    limit: usize,
) -> Result<Vec<EntityKey>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut prefix = [0u8; 3];
    prefix[0] = TAG_TYPE_LIST;
    prefix[1..3].copy_from_slice(&type_id.0.to_be_bytes());
    let mut start = [0u8; 11];
    start[0..3].copy_from_slice(&prefix);
    if let Some(after) = after_exclusive {
        let Some(next_k) = after.0.checked_add(1) else {
            return Ok(Vec::new());
        };
        start[3..11].copy_from_slice(&next_k.to_be_bytes());
    }
    let mut cur = txn.cursor(dbs.adj)?;
    let mut keys = Vec::new();
    let mut entry = cur.seek_ge(&start)?;
    while let Some((k, _)) = entry {
        if k.len() != 11 || k[0..3] != prefix {
            break;
        }
        let ek = EntityKey::from_be_bytes(k[3..11].try_into().expect("8"));
        if let Some(max_k) = max_inclusive
            && ek > max_k
        {
            break;
        }
        keys.push(ek);
        if keys.len() >= limit {
            break;
        }
        entry = cur.next()?;
    }
    Ok(keys)
}

/// Returns the highest `EntityKey` currently present for `type_id`, in O(log N).
pub fn max_type_key<R: KvRead>(txn: &R, dbs: &SystemDbs, type_id: TypeId) -> Result<Option<EntityKey>> {
    let mut prefix = [0u8; 3];
    prefix[0] = TAG_TYPE_LIST;
    prefix[1..3].copy_from_slice(&type_id.0.to_be_bytes());
    let mut upper = [0u8; 11];
    upper[0..3].copy_from_slice(&prefix);
    upper[3..11].copy_from_slice(&u64::MAX.to_be_bytes());
    let mut cur = txn.cursor(dbs.adj)?;
    let entry = match cur.seek_ge(&upper)? {
        Some((k, _)) if k.len() == 11 && k[0..3] == prefix => Some((k, ())),
        Some(_) => cur.prev()?.map(|(k, _)| (k, ())),
        None => cur.last()?.map(|(k, _)| (k, ())),
    };
    if let Some((k, ())) = entry
        && k.len() == 11
        && k[0..3] == prefix
    {
        return Ok(Some(EntityKey::from_be_bytes(k[3..11].try_into().expect("8"))));
    }
    Ok(None)
}

/// Decodes a `PackedRow` (resolving any spilled blobs) directly from primary storage.
pub fn read_packed_row<R: KvRead>(txn: &R, dbs: &SystemDbs, key: EntityKey) -> Result<Option<PackedRow>> {
    let Some(raw) = txn.get(dbs.entities, &key.to_ne_bytes())? else {
        return Ok(None);
    };
    let packed = decode_row(raw, |fid| Ok(txn.get(dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned)))?;
    Ok(Some(packed))
}

/// Scans outgoing edges `(target_entity, weight, kind)` from `src` filtered to `kinds` (empty = all kinds)
/// directly over `adj_out` with zero string allocations!
pub fn scan_adj_out_raw<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    src: EntityKey,
    kinds: &[RelKindId],
) -> Result<Vec<(EntityKey, f64, RelKindId)>> {
    let mut out = Vec::new();
    let mut cur = txn.cursor(dbs.adj)?;
    if kinds.is_empty() {
        let mut prefix = [0u8; 9];
        prefix[0] = TAG_ADJ_OUT;
        prefix[1..9].copy_from_slice(&src.to_be_bytes());
        let mut entry = cur.seek_ge(&prefix)?;
        while let Some((k, _)) = entry {
            if k.len() != 19 || !k.starts_with(&prefix) {
                break;
            }
            let rk = RelKey::from_be_bytes(k[11..19].try_into().expect("8"));
            if let Some(raw) = txn.get(dbs.relations, &rk.to_ne_bytes())? {
                let edge = PackedEdge::decode(rk, raw)?;
                out.push((edge.dst, edge.weight, edge.kind));
            }
            entry = cur.next()?;
        }
    } else {
        let mut seen_kinds: BTreeSet<RelKindId> = BTreeSet::new();
        for &kind in kinds {
            if !seen_kinds.insert(kind) {
                continue;
            }
            let mut prefix = [0u8; 11];
            prefix[0] = TAG_ADJ_OUT;
            prefix[1..9].copy_from_slice(&src.to_be_bytes());
            prefix[9..11].copy_from_slice(&kind.0.to_be_bytes());
            let mut entry = cur.seek_ge(&prefix)?;
            while let Some((k, _)) = entry {
                if k.len() != 19 || !k.starts_with(&prefix) {
                    break;
                }
                let rk = RelKey::from_be_bytes(k[11..19].try_into().expect("8"));
                if let Some(raw) = txn.get(dbs.relations, &rk.to_ne_bytes())? {
                    let edge = PackedEdge::decode(rk, raw)?;
                    out.push((edge.dst, edge.weight, edge.kind));
                }
                entry = cur.next()?;
            }
        }
    }
    Ok(out)
}

/// Scans incoming edges `(source_entity, weight, kind)` into `dst` filtered to `kinds` (empty = all kinds)
/// directly over `adj_in` with zero string allocations!
pub fn scan_adj_in_raw<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    dst: EntityKey,
    kinds: &[RelKindId],
) -> Result<Vec<(EntityKey, f64, RelKindId)>> {
    let mut out = Vec::new();
    let mut cur = txn.cursor(dbs.adj)?;
    if kinds.is_empty() {
        let mut prefix = [0u8; 9];
        prefix[0] = TAG_ADJ_IN;
        prefix[1..9].copy_from_slice(&dst.to_be_bytes());
        let mut entry = cur.seek_ge(&prefix)?;
        while let Some((k, _)) = entry {
            if k.len() != 19 || !k.starts_with(&prefix) {
                break;
            }
            let rk = RelKey::from_be_bytes(k[11..19].try_into().expect("8"));
            if let Some(raw) = txn.get(dbs.relations, &rk.to_ne_bytes())? {
                let edge = PackedEdge::decode(rk, raw)?;
                out.push((edge.src, edge.weight, edge.kind));
            }
            entry = cur.next()?;
        }
    } else {
        let mut seen_kinds: BTreeSet<RelKindId> = BTreeSet::new();
        for &kind in kinds {
            if !seen_kinds.insert(kind) {
                continue;
            }
            let mut prefix = [0u8; 11];
            prefix[0] = TAG_ADJ_IN;
            prefix[1..9].copy_from_slice(&dst.to_be_bytes());
            prefix[9..11].copy_from_slice(&kind.0.to_be_bytes());
            let mut entry = cur.seek_ge(&prefix)?;
            while let Some((k, _)) = entry {
                if k.len() != 19 || !k.starts_with(&prefix) {
                    break;
                }
                let rk = RelKey::from_be_bytes(k[11..19].try_into().expect("8"));
                if let Some(raw) = txn.get(dbs.relations, &rk.to_ne_bytes())? {
                    let edge = PackedEdge::decode(rk, raw)?;
                    out.push((edge.src, edge.weight, edge.kind));
                }
                entry = cur.next()?;
            }
        }
    }
    Ok(out)
}

/// Scans all `PackedEdge`s of a given `RelKindId` from `adj_by_kind` + `relations`.
pub fn scan_edges_by_kind<R: KvRead>(txn: &R, dbs: &SystemDbs, kind: RelKindId) -> Result<Vec<PackedEdge>> {
    let mut prefix = [0u8; 3];
    prefix[0] = TAG_ADJ_BY_KIND;
    prefix[1..3].copy_from_slice(&kind.0.to_be_bytes());
    let mut cur = txn.cursor(dbs.adj)?;
    let mut out = Vec::new();
    let mut entry = cur.seek_ge(&prefix)?;
    while let Some((k, _)) = entry {
        if k.len() != 11 || k[0..3] != prefix {
            break;
        }
        let rk = RelKey::from_be_bytes(k[3..11].try_into().expect("8"));
        if let Some(raw) = txn.get(dbs.relations, &rk.to_ne_bytes())? {
            out.push(PackedEdge::decode(rk, raw)?);
        }
        entry = cur.next()?;
    }
    Ok(out)
}

pub fn read_raw_stored_row<R: KvRead>(
    txn: &R,
    schema: &CompiledSchema,
    dbs: &SystemDbs,
    key: EntityKey,
) -> Result<Option<StoredRow>> {
    let Some(raw) = txn.get(dbs.entities, &key.to_ne_bytes())? else {
        return Ok(None);
    };
    let packed = decode_row(raw, |fid| Ok(txn.get(dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned)))?;
    let tdef = schema
        .type_by_id(packed.header.type_id)
        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "row references unknown TypeId"))?;

    let mut fields = BTreeMap::new();
    for (fid, val) in packed.fields {
        if let Some(fdef) = tdef.field_by_id(fid) {
            fields.insert(fdef.name.clone(), val);
        }
    }
    for (k, v) in packed.extra {
        fields.insert(k, v);
    }

    let trashed = packed.header.is_trashed();
    Ok(Some(StoredRow {
        key,
        id: packed.header.ext_id,
        type_name: tdef.name.clone(),
        type_id: tdef.id,
        trashed,
        created_at_ms: packed.header.created_at_ms,
        updated_at_ms: packed.header.updated_at_ms,
        trashed_at_ms: packed.header.trashed_at_ms,
        fields,
    }))
}

pub fn read_stored_row<R: KvRead>(
    txn: &R,
    schema: &CompiledSchema,
    dbs: &SystemDbs,
    key: EntityKey,
) -> Result<Option<StoredRow>> {
    let Some(raw) = txn.get(dbs.entities, &key.to_ne_bytes())? else {
        return Ok(None);
    };
    let packed = decode_row(raw, |fid| Ok(txn.get(dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned)))?;
    let tdef = schema
        .type_by_id(packed.header.type_id)
        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "row references unknown TypeId"))?;

    let mut ref_cache: FxHashMap<u64, String> = FxHashMap::default();
    let mut fields = BTreeMap::new();
    for (fid, val) in packed.fields {
        if let Some(fdef) = tdef.field_by_id(fid) {
            let resolved = resolve_refs_to_strings(txn, dbs, val, &mut ref_cache)?;
            fields.insert(fdef.name.clone(), resolved);
        }
    }
    for (k, v) in packed.extra {
        fields.insert(k, v);
    }

    let trashed = packed.header.is_trashed();
    Ok(Some(StoredRow {
        key,
        id: packed.header.ext_id,
        type_name: tdef.name.clone(),
        type_id: tdef.id,
        trashed,
        created_at_ms: packed.header.created_at_ms,
        updated_at_ms: packed.header.updated_at_ms,
        trashed_at_ms: packed.header.trashed_at_ms,
        fields,
    }))
}

fn resolve_refs_to_strings<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    val: CanonicalValue,
    cache: &mut FxHashMap<u64, String>,
) -> Result<CanonicalValue> {
    match val {
        CanonicalValue::EntityRef(k) => {
            if let Some(ext) = cache.get(&k) {
                return Ok(CanonicalValue::String(ext.clone()));
            }
            let raw = txn
                .get(dbs.entities, &k.to_ne_bytes())?
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, format!("unresolvable EntityRef({k})")))?;
            let ext = decode_row_header(raw)?.0.ext_id;
            cache.insert(k, ext.clone());
            Ok(CanonicalValue::String(ext))
        }
        CanonicalValue::DanglingRef(ext) => Ok(CanonicalValue::String(ext)),
        CanonicalValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(resolve_refs_to_strings(txn, dbs, item, cache)?);
            }
            Ok(CanonicalValue::Array(out))
        }
        other => Ok(other),
    }
}

fn read_adj_edges<R: KvRead>(
    txn: &R,
    schema: &CompiledSchema,
    dbs: &SystemDbs,
    tag: u8,
    node: EntityKey,
    kind_filter: Option<RelKindId>,
) -> Result<Vec<StoredEdge>> {
    let mut prefix = Vec::with_capacity(11);
    prefix.push(tag);
    prefix.extend_from_slice(&node.to_be_bytes());
    if let Some(k) = kind_filter {
        prefix.extend_from_slice(&k.0.to_be_bytes());
    }
    let mut cur = txn.cursor(dbs.adj)?;
    let mut out = Vec::new();
    let mut entry = cur.seek_ge(&prefix)?;
    while let Some((k, _)) = entry {
        if !k.starts_with(&prefix) || k.len() != 19 {
            break;
        }
        let rel_key = RelKey::from_be_bytes(k[11..19].try_into().expect("8"));
        if let Some(raw) = txn.get(dbs.relations, &rel_key.to_ne_bytes())? {
            let edge = PackedEdge::decode(rel_key, raw)?;
            let kind_name =
                schema.relation_by_id(edge.kind).map_or_else(|| format!("rel#{}", edge.kind.0), |r| r.name.clone());
            out.push(StoredEdge {
                key: edge.key,
                kind_name,
                kind_id: edge.kind,
                src: edge.src,
                dst: edge.dst,
                weight: edge.weight,
                created_at_ms: edge.created_at_ms,
                info: edge.info,
            });
        }
        entry = cur.next()?;
    }
    Ok(out)
}
