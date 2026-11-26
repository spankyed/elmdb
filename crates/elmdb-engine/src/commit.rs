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
use crate::ids::{EntityKey, FieldId, FieldMask, RelKey, RelKindId, TypeId, TypeMask};
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
    gate: DurabilityGate<S, V>,
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
struct DeltaBatch {
    entities: BTreeMap<EntityKey, EntityDelta>,
    edges: BTreeMap<RelKey, EdgeDelta>,
    touched_types: TypeMask,
    touched_rel_kinds: BTreeSet<RelKindId>,
    tombstones: Vec<(EntityKey, TypeId, String)>,
}

impl DeltaBatch {
    fn new() -> Self {
        Self {
            entities: BTreeMap::new(),
            edges: BTreeMap::new(),
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
        self.touched_types.insert(type_id);
        match self.entities.get_mut(&key) {
            Some(existing) => {
                existing.changed_fields.union_with(&changed_fields);
                existing.liveness_changed |= liveness_changed;
                existing.after = after;
            }
            None => {
                self.entities
                    .insert(key, EntityDelta { key, type_id, before, after, changed_fields, liveness_changed });
            }
        }
    }

    fn record_edge(&mut self, key: RelKey, kind: RelKindId, before: Option<PackedEdge>, after: Option<PackedEdge>) {
        self.touched_rel_kinds.insert(kind);
        match self.edges.get_mut(&key) {
            Some(existing) => existing.after = after,
            None => {
                self.edges.insert(key, EdgeDelta { key, kind, before, after });
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.edges.is_empty()
    }
}

/// Active write-transaction context exposed to `transact()` and `beforeCommit` hooks.
pub struct TxnCtx<'a, S: KvStore, V: Vfs, C: Clock> {
    engine: &'a Engine<S, V, C>,
    txn: GatedWriteTxn<'a, S, V>,
    schema: Arc<CompiledSchema>,
    commit_seq: u64,
    now_ms: i64,
    /// Accumulated deltas for the whole transaction (used for summary + `commit_log`).
    total_deltas: DeltaBatch,
    /// Types touched in the current round (triggers `beforeCommit` cascade).
    round_touched_types: TypeMask,
    /// Pending un-flushed deltas (flushed to `ProjectionSink` after every statement / batch).
    pending_deltas: DeltaBatch,
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
        let (gate, report) = DurabilityGate::open(store, vfs, dir, mode)?;
        let (dbs, schema, pool) = {
            let mut txn = gate.begin_write()?;
            let dbs = SystemDbs::open_or_create(&mut txn)?;
            let schema = match txn.get(dbs.meta, META_SCHEMA_KEY)? {
                Some(bytes) => serde_json::from_slice::<CompiledSchema>(bytes)
                    .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt meta schema: {e}")))?,
                None => {
                    let empty = CompiledSchema::empty();
                    let encoded =
                        serde_json::to_vec(&empty).map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;
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
        };

        let engine = Self {
            gate,
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

    /// Flushes LMDB and writes the verified 4 KiB durability sidecar slot.
    pub fn sync(&self) -> Result<()> {
        self.gate.sync()?;
        Ok(())
    }

    /// Compiles and installs a new schema, retiring dropped index slots via O(1) `truncate`
    /// without calling `mdb_dbi_close()`.
    pub fn set_schema(&self, raw: &RawSchema) -> Result<(Arc<CompiledSchema>, SchemaDiff)> {
        let mut state = self.schema_state.write();
        let mut txn = self.gate.begin_write()?;
        self.refresh_schema_locked(&txn, &mut state)?;

        let mut next_pool = state.pool.clone();
        let (compiled, diff) = CompiledSchema::compile(raw, Some(&state.schema), &mut next_pool)?;

        // Truncate retired sub-DB slots in O(1) before recycling them into `meta.subdb_pool`.
        for &slot in &diff.retired_slots {
            let db = txn.open_subdb(&slot.name(), DbFlags::DEFAULT)?;
            txn.truncate(db)?;
        }
        // Pre-open newly allocated slots at root write txn so readers never need `mdb_dbi_open`.
        for &idx_id in &diff.added_indexes {
            if let Some(def) = compiled.index_by_id(idx_id) {
                let _ = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
            }
        }

        self.projections.on_schema_change(&mut txn, &compiled, &self.dbs, &diff)?;

        let seq = self.next_commit_seq(&txn)?;
        persist_schema_and_pool(&mut txn, &self.dbs, &compiled, &next_pool, seq)?;
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

        let arc = Arc::new(compiled);
        state.schema = Arc::clone(&arc);
        state.pool = next_pool;
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
        let mut state = self.schema_state.write();
        let mut txn = self.gate.begin_write()?;
        self.refresh_schema_locked(&txn, &mut state)?;

        let type_id = state
            .schema
            .type_by_name(old_type)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{old_type}'")))?
            .id;
        let next = state.schema.rename_type(old_type, new_type)?;

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
                    let enc = encode_row(&row)?;
                    txn.put(self.dbs.entities, &key.to_ne_bytes(), &enc.row_bytes)?;
                }
            }
        }

        let seq = self.next_commit_seq(&txn)?;
        persist_schema_and_pool(&mut txn, &self.dbs, &next, &state.pool, seq)?;
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

        let arc = Arc::new(next);
        state.schema = Arc::clone(&arc);
        Ok(arc)
    }

    fn apply_schema_rename<F>(&self, f: F) -> Result<Arc<CompiledSchema>>
    where
        F: FnOnce(&CompiledSchema) -> Result<CompiledSchema>,
    {
        let mut state = self.schema_state.write();
        let mut txn = self.gate.begin_write()?;
        self.refresh_schema_locked(&txn, &mut state)?;
        let next = f(&state.schema)?;
        let seq = self.next_commit_seq(&txn)?;
        persist_schema_and_pool(&mut txn, &self.dbs, &next, &state.pool, seq)?;
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
        let arc = Arc::new(next);
        state.schema = Arc::clone(&arc);
        Ok(arc)
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
        let schema = self.load_fresh_schema_for_write()?;
        let txn = self.gate.begin_write()?;
        let commit_seq = self.next_commit_seq(&txn)?;
        let now_ms = self.clock.now_ms();

        let mut ctx = TxnCtx {
            engine: self,
            txn,
            schema,
            commit_seq,
            now_ms,
            total_deltas: DeltaBatch::new(),
            round_touched_types: TypeMask::empty(),
            pending_deltas: DeltaBatch::new(),
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
    }

    /// Executes a synchronous read-modify-write transaction with native LMDB nested savepoints
    /// and a final bounded `beforeCommit` cascade.
    pub fn transact<T, F>(&self, f: F) -> Result<(T, CommitReceipt)>
    where
        F: FnOnce(&mut TxnCtx<'_, S, V, C>) -> Result<T>,
    {
        let schema = self.load_fresh_schema_for_write()?;
        let txn = self.gate.begin_write()?;
        let commit_seq = self.next_commit_seq(&txn)?;
        let now_ms = self.clock.now_ms();

        let mut ctx = TxnCtx {
            engine: self,
            txn,
            schema,
            commit_seq,
            now_ms,
            total_deltas: DeltaBatch::new(),
            round_touched_types: TypeMask::empty(),
            pending_deltas: DeltaBatch::new(),
        };

        let value = f(&mut ctx)?;
        ctx.flush_projections()?;
        ctx.run_before_commit_cascade()?;
        ctx.finalize_commit_log()?;
        let (receipt, _) = ctx.commit_with_summary(false)?;
        Ok((value, receipt))
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
        let Some(key_bytes) = ro.get(self.dbs.ext_id, ext_id.as_bytes())? else {
            return Ok(None);
        };
        let key = EntityKey(u64::from_le_bytes(
            key_bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "corrupt ext_id key bytes"))?,
        ));
        let Some(raw) = ro.get(self.dbs.entities, &key.to_ne_bytes())? else {
            return Ok(None);
        };
        let (hdr, _) = decode_row_header(raw)?;
        let tdef = schema
            .type_by_id(hdr.type_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "row references unknown TypeId"))?;
        if let Some(fdef) = tdef.field_by_name(field_name) {
            let val = decode_single_field(raw, fdef.id, |fid| {
                Ok(ro.get(self.dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned))
            })?;
            return Ok(val);
        }
        // Fallback for dynamic `additionalFields: allow` extra fields.
        let full = decode_row(raw, |fid| Ok(ro.get(self.dbs.blobs, &blob_key(key, fid))?.map(ToOwned::to_owned)))?;
        Ok(full.extra.get(field_name).cloned())
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
        let Some(key_bytes) = ro.get(self.dbs.ext_id, ext_id.as_bytes())? else {
            return Ok(Vec::new());
        };
        let src = EntityKey(u64::from_le_bytes(key_bytes.try_into().expect("8")));
        let kind_filter = match rel_kind {
            Some(name) => Some(
                schema
                    .relation_by_name(name)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownRelation, format!("unknown relation '{name}'")))?
                    .id,
            ),
            None => None,
        };
        read_adj_edges(&ro, &schema, &self.dbs, TAG_ADJ_OUT, src, kind_filter)
    }

    /// Lists incoming relations to `ext_id`, optionally filtered by `rel_kind`.
    pub fn relations_in(&self, ext_id: &str, rel_kind: Option<&str>) -> Result<Vec<StoredEdge>> {
        let (ro, schema) = self.begin_read_pinned()?;
        let Some(key_bytes) = ro.get(self.dbs.ext_id, ext_id.as_bytes())? else {
            return Ok(Vec::new());
        };
        let dst = EntityKey(u64::from_le_bytes(key_bytes.try_into().expect("8")));
        let kind_filter = match rel_kind {
            Some(name) => Some(
                schema
                    .relation_by_name(name)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownRelation, format!("unknown relation '{name}'")))?
                    .id,
            ),
            None => None,
        };
        read_adj_edges(&ro, &schema, &self.dbs, TAG_ADJ_IN, dst, kind_filter)
    }

    /// Reads persisted cross-process commit summaries with `seq > since_seq`.
    /// Returns `Ok(None)` if `> 4,096` commits elapsed and the caller must perform full invalidation.
    pub fn read_commit_summaries_since(&self, since_seq: u64) -> Result<Option<Vec<CrossProcCommitSummary>>> {
        let (ro, _) = self.begin_read_pinned()?;
        let tip = read_commit_seq(&ro, &self.dbs)?;
        if tip <= since_seq {
            return Ok(Some(Vec::new()));
        }
        if tip.saturating_sub(since_seq) > COMMIT_LOG_WINDOW {
            return Ok(None);
        }
        let mut start_key = [0u8; 9];
        start_key[0] = TAG_COMMIT_SUMMARY;
        start_key[1..9].copy_from_slice(&(since_seq.saturating_add(1)).to_be_bytes());

        let mut cur = ro.cursor(self.dbs.commit_log)?;
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
        Ok(Some(out))
    }

    /// Opens a read-only transaction pinned to the exact `Arc<CompiledSchema>` for its MVCC snapshot,
    /// seamlessly refreshing the cached schema if another process advanced `schema_epoch`.
    pub fn begin_read_pinned(&self) -> Result<(S::RoTxn<'_>, Arc<CompiledSchema>)> {
        let ro = self.gate.begin_read()?;
        let disk_epoch = read_schema_epoch(&ro, &self.dbs)?;
        let cached = Arc::clone(&self.schema_state.read().schema);
        if cached.epoch == disk_epoch {
            return Ok((ro, cached));
        }
        // Cross-process schema advance detected: refresh cache under L1 write lock.
        let mut state = self.schema_state.write();
        self.refresh_schema_locked(&ro, &mut state)?;
        Ok((ro, Arc::clone(&state.schema)))
    }

    fn load_fresh_schema_for_write(&self) -> Result<Arc<CompiledSchema>> {
        let (_, schema) = self.begin_read_pinned()?;
        Ok(schema)
    }

    fn refresh_schema_locked<R: KvRead>(&self, txn: &R, state: &mut SchemaState) -> Result<()> {
        let disk_epoch = read_schema_epoch(txn, &self.dbs)?;
        if state.schema.epoch == disk_epoch {
            return Ok(());
        }
        let bytes = txn
            .get(self.dbs.meta, META_SCHEMA_KEY)?
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing meta schema"))?;
        let schema: CompiledSchema = serde_json::from_slice(bytes)
            .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("corrupt meta schema: {e}")))?;
        let pool_bytes = txn
            .get(self.dbs.meta, META_POOL_KEY)?
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing meta subdb_pool"))?;
        let pool = SubDbPool::decode(pool_bytes)?;
        state.schema = Arc::new(schema);
        state.pool = pool;
        Ok(())
    }

    fn next_commit_seq<R: KvRead>(&self, txn: &R) -> Result<u64> {
        Ok(read_commit_seq(txn, &self.dbs)?.saturating_add(1))
    }
}

impl<'a, S: KvStore, V: Vfs, C: Clock> TxnCtx<'a, S, V, C> {
    pub fn schema(&self) -> &Arc<CompiledSchema> {
        &self.schema
    }

    pub fn get_row(&self, ext_id: &str) -> Result<Option<StoredRow>> {
        let Some(key_bytes) = self.txn.get(self.engine.dbs.ext_id, ext_id.as_bytes())? else {
            return Ok(None);
        };
        let key = EntityKey(u64::from_le_bytes(key_bytes.try_into().expect("8")));
        read_stored_row(&self.txn, &self.schema, &self.engine.dbs, key)
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

    /// Executes a single `WireOp` inside this transaction and immediately flushes its coalesced
    /// delta to `ProjectionSink` so subsequent reads inside the same transaction see fresh indexes.
    pub fn exec(&mut self, op: &WireOp) -> Result<Option<String>> {
        let mut slots = Vec::new();
        self.apply_op(op, &mut slots)?;
        self.flush_projections()?;
        Ok(slots.into_iter().next().map(|(_, id)| id))
    }

    /// Executes `f` inside a native LMDB nested savepoint inside this transaction.
    pub fn savepoint<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        let depth = self.txn.begin_savepoint()?;
        match f(self) {
            Ok(v) => {
                self.txn.commit_savepoint(depth)?;
                Ok(v)
            }
            Err(err) => {
                while self.txn.savepoint_depth() >= depth {
                    let d = self.txn.savepoint_depth();
                    let _ = self.txn.abort_savepoint(d);
                }
                Err(err)
            }
        }
    }

    fn flush_projections(&mut self) -> Result<()> {
        if self.pending_deltas.is_empty() {
            return Ok(());
        }
        let batch = std::mem::replace(&mut self.pending_deltas, DeltaBatch::new());
        let entity_vec: Vec<EntityDelta> = batch.entities.values().cloned().collect();
        let edge_vec: Vec<EdgeDelta> = batch.edges.values().cloned().collect();

        self.engine.projections.apply_deltas(&mut self.txn, &self.schema, &self.engine.dbs, &entity_vec, &edge_vec)?;

        self.round_touched_types.union_with(&batch.touched_types);
        for d in entity_vec {
            self.total_deltas.record_entity(d.key, d.type_id, d.before, d.after, d.changed_fields, d.liveness_changed);
        }
        for e in edge_vec {
            self.total_deltas.record_edge(e.key, e.kind, e.before, e.after);
        }
        self.total_deltas.tombstones.extend(batch.tombstones);
        Ok(())
    }

    fn run_before_commit_cascade(&mut self) -> Result<()> {
        for round in 0..=MAX_HOOK_ROUNDS {
            if self.round_touched_types.is_empty() {
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
                return Ok(());
            }
            if round == MAX_HOOK_ROUNDS {
                return Err(ElmdbError::new(
                    ErrorCode::HookCycleLimit,
                    format!("beforeCommit hook cascade exceeded {MAX_HOOK_ROUNDS} rounds"),
                ));
            }
            self.round_touched_types = TypeMask::empty();
            let depth = self.txn.begin_savepoint()?;
            for cb in matching {
                if let Err(err) = cb(self) {
                    while self.txn.savepoint_depth() >= depth {
                        let d = self.txn.savepoint_depth();
                        let _ = self.txn.abort_savepoint(d);
                    }
                    return Err(err);
                }
            }
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
            Some(bytes) => u64::from_le_bytes(bytes.try_into().expect("8")) + 1,
            None => 1,
        };
        self.txn.put(self.engine.dbs.sequences, SEQ_ENTITY_KEY, &next.to_le_bytes())?;
        Ok(EntityKey(next))
    }

    fn alloc_rel_key(&mut self) -> Result<RelKey> {
        let next = match self.txn.get(self.engine.dbs.sequences, SEQ_REL_KEY)? {
            Some(bytes) => u64::from_le_bytes(bytes.try_into().expect("8")) + 1,
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
                    if let Some(fdef) = tdef.field_by_name(fname) {
                        let resolved = self.validate_and_resolve_field(fdef, raw_val, slots)?;
                        changed_fields.insert(fdef.id);
                        declared.insert(fdef.id, resolved);
                    } else if tdef.additional_fields {
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
                    if let Some(fdef) = tdef.field_by_name(fname) {
                        let resolved = self.validate_and_resolve_field(fdef, raw_val, slots)?;
                        if next.fields.get(&fdef.id) != Some(&resolved) {
                            changed_fields.insert(fdef.id);
                            next.fields.insert(fdef.id, resolved);
                        }
                    } else if tdef.additional_fields {
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
                let tdef = self.require_active_type(old_row.header.type_id)?;

                // 1. Enforce `ref_in` `onDelete` policies (Restrict / SetNull / AllowDangling).
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
                self.txn.del(self.engine.dbs.adj, &type_list_key(tdef.id, key))?;
                self.txn.del(self.engine.dbs.entities, &key.to_ne_bytes())?;

                self.pending_deltas.tombstones.push((key, tdef.id, old_row.header.ext_id.clone()));
                self.pending_deltas.record_entity(key, tdef.id, Some(old_row), None, FieldMask::empty(), true);
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

                if !rdef.endpoints.is_empty() && !rdef.endpoints.contains(&(src_hdr.type_id, dst_hdr.type_id)) {
                    return Err(ElmdbError::new(
                        ErrorCode::EndpointMismatch,
                        format!("relation '{}' does not allow these endpoint types", rdef.name),
                    ));
                }

                // Enforce cardinality via O(1) adjacency cursor checks.
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
                        f
                    }
                    None => 1.0,
                };

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
                self.pending_deltas.record_edge(rel_key, rdef.id, None, Some(edge));
                self.pending_deltas.touched_types.insert(src_hdr.type_id);
                self.pending_deltas.touched_types.insert(dst_hdr.type_id);
                Ok(())
            }
            WireOp::Unlink { source, rel_kind, target } => {
                let src = self.resolve_target(source, slots)?;
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
                    Some(t) => Some(self.resolve_target(t, slots)?),
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
            let unchanged =
                old.is_some_and(|o| prev_blobs.contains(&fid) && o.fields.get(&fid) == next.fields.get(&fid));
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
            | (FieldKind::Timestamp, CanonicalValue::Timestamp { .. })
            | (FieldKind::Object, CanonicalValue::Object(_))
            | (FieldKind::Any, _) => Ok(val.clone()),
            (FieldKind::Timestamp, CanonicalValue::Int(ms)) => Ok(CanonicalValue::Timestamp { ms: *ms, ns: 0 }),
            (FieldKind::Number, CanonicalValue::Int(_) | CanonicalValue::Float(_)) => Ok(val.clone()),
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
            (FieldKind::Ref { .. }, CanonicalValue::DanglingRef(ext)) => Ok(CanonicalValue::DanglingRef(ext.clone())),
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

        // Group by referrer so each referrer row is rewritten at most once.
        let mut by_referrer: BTreeMap<EntityKey, Vec<FieldId>> = BTreeMap::new();
        for (r, fid) in refs {
            if r != target {
                by_referrer.entry(r).or_default().push(fid);
            }
        }

        for (referrer_key, fids) in by_referrer {
            let old_row = self.load_packed_row(referrer_key)?;
            let tdef = self
                .schema
                .type_by_id(old_row.header.type_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing TypeDef for referrer"))?
                .clone();

            let mut next = old_row.clone();
            let mut changed_fields = FieldMask::empty();

            for fid in fids {
                let fdef = tdef
                    .field_by_id(fid)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing FieldDef for ref_in"))?;
                let policy = fdef.kind.ref_policy().map_or(OnDeletePolicy::Restrict, |(_, p)| p);
                match policy {
                    OnDeletePolicy::Restrict => {
                        return Err(ElmdbError::new(
                            ErrorCode::Referenced,
                            format!(
                                "cannot destroy '{}': referenced by '{}' field '{}.{}'",
                                target_ext_id, old_row.header.ext_id, tdef.name, fdef.name
                            ),
                        ));
                    }
                    OnDeletePolicy::SetNull => match next.fields.get_mut(&fid) {
                        Some(CanonicalValue::EntityRef(k)) if *k == target.0 => {
                            if fdef.nullable {
                                next.fields.insert(fid, CanonicalValue::Null);
                            } else {
                                next.fields.remove(&fid);
                            }
                            changed_fields.insert(fid);
                        }
                        Some(CanonicalValue::Array(items)) => {
                            let before_len = items.len();
                            items.retain(|item| !matches!(item, CanonicalValue::EntityRef(k) if *k == target.0));
                            if items.len() != before_len {
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
        if let Ok(hdr) = self.load_row_header(edge.src) {
            self.pending_deltas.touched_types.insert(hdr.type_id);
        }
        if let Ok(hdr) = self.load_row_header(edge.dst) {
            self.pending_deltas.touched_types.insert(hdr.type_id);
        }
        self.pending_deltas.record_edge(rel_key, edge.kind, Some(edge), None);
        Ok(())
    }

    fn check_cardinality(&self, rdef: &crate::catalog::RelKindDef, src: EntityKey, dst: EntityKey) -> Result<()> {
        match rdef.cardinality {
            Cardinality::ManyToMany => Ok(()),
            Cardinality::ManyToOne => {
                if self.has_adj_with_other(TAG_ADJ_OUT, src, rdef.id, dst)? {
                    return Err(ElmdbError::new(
                        ErrorCode::CardinalityViolation,
                        format!("relation '{}' is many-to-one (source already linked elsewhere)", rdef.name),
                    ));
                }
                Ok(())
            }
            Cardinality::OneToMany => {
                if self.has_adj_with_other(TAG_ADJ_IN, dst, rdef.id, src)? {
                    return Err(ElmdbError::new(
                        ErrorCode::CardinalityViolation,
                        format!("relation '{}' is one-to-many (target already linked from elsewhere)", rdef.name),
                    ));
                }
                Ok(())
            }
            Cardinality::OneToOne => {
                if self.has_adj_with_other(TAG_ADJ_OUT, src, rdef.id, dst)?
                    || self.has_adj_with_other(TAG_ADJ_IN, dst, rdef.id, src)?
                {
                    return Err(ElmdbError::new(
                        ErrorCode::CardinalityViolation,
                        format!("relation '{}' is one-to-one", rdef.name),
                    ));
                }
                Ok(())
            }
        }
    }

    fn has_adj_with_other(&self, tag: u8, node: EntityKey, kind: RelKindId, allowed_far: EntityKey) -> Result<bool> {
        let mut prefix = [0u8; 11];
        prefix[0] = tag;
        prefix[1..9].copy_from_slice(&node.to_be_bytes());
        prefix[9..11].copy_from_slice(&kind.0.to_be_bytes());

        let mut cur = self.txn.cursor(self.engine.dbs.adj)?;
        let mut entry = cur.seek_ge(&prefix)?;
        while let Some((k, v)) = entry {
            if k.len() != 19 || k[0..11] != prefix {
                break;
            }
            let far = EntityKey::from_be_bytes(v[0..8].try_into().expect("8"));
            if far != allowed_far {
                return Ok(true);
            }
            entry = cur.next()?;
        }
        Ok(false)
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

fn read_commit_seq<R: KvRead>(txn: &R, dbs: &SystemDbs) -> Result<u64> {
    match txn.get(dbs.meta, META_COMMIT_SEQ_KEY)? {
        Some(bytes) => Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "corrupt commit_seq"))?,
        )),
        None => Ok(0),
    }
}

fn read_schema_epoch<R: KvRead>(txn: &R, dbs: &SystemDbs) -> Result<u64> {
    match txn.get(dbs.meta, META_SCHEMA_EPOCH_KEY)? {
        Some(bytes) => Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "corrupt schema_epoch"))?,
        )),
        None => Ok(0),
    }
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

fn scan_type_keys<R: KvRead>(txn: &R, dbs: &SystemDbs, type_id: TypeId) -> Result<Vec<EntityKey>> {
    let mut prefix = [0u8; 3];
    prefix[0] = TAG_TYPE_LIST;
    prefix[1..3].copy_from_slice(&type_id.0.to_be_bytes());
    let mut cur = txn.cursor(dbs.adj)?;
    let mut keys = Vec::new();
    let mut entry = cur.seek_ge(&prefix)?;
    while let Some((k, _)) = entry {
        if k.len() != 11 || k[0..3] != prefix {
            break;
        }
        keys.push(EntityKey::from_be_bytes(k[3..11].try_into().expect("8")));
        entry = cur.next()?;
    }
    Ok(keys)
}

fn read_stored_row<R: KvRead>(
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
