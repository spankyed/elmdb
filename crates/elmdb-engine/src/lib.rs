#![forbid(unsafe_code)]

//! Clean-room elmdb v2 engine core: integer catalog, O(1) schema evolution,
//! hybrid packed-row + 4 KiB blob storage, referential integrity (`ref_in` + `onDelete`),
//! coalesced `EntityDelta`/`EdgeDelta` commit pipeline, bounded `beforeCommit` cascade,
//! and persisted 4,096-commit cross-process summary log.

pub mod catalog;
pub mod commit;
pub mod ids;
pub mod row;

pub use catalog::{
    Cardinality, CompiledSchema, FieldDef, FieldKind, IndexDef, IndexKind, OnDeletePolicy, RawEndpoint, RawField,
    RawFieldKind, RawIndex, RawIndexKind, RawRelation, RawSchema, RawType, RelKindDef, SchemaDiff, TypeDef,
};
pub use commit::{
    BatchOutcome, BeforeCommitHook, CommitDeltaSummary, CrossProcCommitSummary, EdgeDelta, Engine, EntityDelta,
    HookCtx, HookRoundSnapshot, MAX_HOOK_ROUNDS, MaintReceipt, ProjectionSink, StoredEdge, StoredRow, SystemDbs,
    TxnCtx, get_counter_in_txn, list_counters_in_txn, lookup_ext_id_in_txn, max_type_key, next_counter_in_txn,
    read_commit_seq, read_commit_summaries_in_txn, read_maint_seq, read_packed_row, read_raw_stored_row,
    read_relations_in_in_txn, read_relations_out_in_txn, read_schema_epoch, read_stored_row, scan_adj_in_raw,
    scan_adj_out_raw, scan_edges_by_kind, scan_type_keys, scan_type_keys_in_range, set_counter_in_txn,
};
pub use ids::{EntityKey, FieldId, FieldMask, IndexId, RelKey, RelKindId, RoleId, TypeId, TypeMask};
pub use row::{INLINE_BLOB_THRESHOLD, MAX_EXT_ID_BYTES, MAX_INLINE_ROW_BYTES, PackedEdge, PackedRow, RowHeader};
