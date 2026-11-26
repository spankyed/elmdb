#![forbid(unsafe_code)]

//! Clean-room **elmdb v2** unified Rust facade.
//!
//! Composes the 6-layer clean-room engine (`elmdb-storage`, `elmdb-lmdb`, `elmdb-wire`, `elmdb-engine`,
//! `elmdb-projections`, `elmdb-vec`, `elmdb-query`, `elmdb-invalidation`) behind a single ergonomic
//! `Database` handle and versioned `protocol` boundary types.

pub mod db;
pub mod diagnostics;
pub mod dump;
pub mod protocol;

pub use db::{BackupInto, Database, OpenOptions};
pub use elmdb_engine::{
    BatchOutcome, BeforeCommitHook, Cardinality, CompiledSchema, EntityKey, FieldId, FieldMask, HookCtx, IndexId,
    IndexKind, OnDeletePolicy, RawEndpoint, RawField, RawFieldKind, RawIndex, RawIndexKind, RawRelation, RawSchema,
    RawType, RelKindId, SchemaDiff, StoredEdge, StoredRow, TxnCtx, TypeId, TypeMask,
};
pub use elmdb_invalidation::{
    InvalidationStats, LiveQueryHub, NextDue, TickResult, WatchId, WatchMode, WatchPolicy, hash_result_128,
};
pub use elmdb_projections::{BuildPolicy, BuildState};
pub use elmdb_query::{
    Cursor, EdgeDir, ExecBudgets, ExplainPlan, FilterExpr, FuseMethod, GraphAnalyzeSpec, LogicalNode, LogicalPlan,
    OrderDir, OrderSpec, ProjectedRow, QueryEngine, QueryOutput, QueryResult, ScoreSpec, SourceSpec, TerminalSpec,
    TraverseSpec,
};
pub use elmdb_storage::DurabilityMode;
pub use elmdb_wire::{
    CanonicalValue, CommitReturnFlags, ElmdbError as Error, EntityTarget, ErrorCode, Result, WireOp, decode_batch,
    encode_batch,
};
pub use protocol::{
    BatchResult, CheckReport, DatabaseStats, DumpReport, Hit, IndexStatus, OpenReport, QueryAnswer, QueryAnswerBody,
    QueryFilter, QueryScore, QuerySource, QuerySpec, QueryStep, QueryTerminal, RelationRow, Row, SCHEMA_LANGUAGE,
    Schema, SchemaChange, WatchTick,
};
