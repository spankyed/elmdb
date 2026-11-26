//! Unified 8-operator algebraic query IR (`LogicalPlan`) and typed expressions.

use elmdb_engine::{EntityKey, FieldId, FieldMask, IndexId, RelKindId, TypeId};
use elmdb_wire::CanonicalValue;
use serde::{Deserialize, Serialize};

/// Edge traversal / adjacency direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeDir {
    Out,
    In,
    Both,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderDir {
    Asc,
    Desc,
}

/// Source set feeding a query plan (Operator #1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    Type { type_id: TypeId, include_trashed: bool },
    Types { type_ids: Vec<TypeId>, include_trashed: bool },
    AllEntities { include_trashed: bool },
    LiteralKeys(Vec<EntityKey>),
}

/// Unified boolean predicate tree (Operator #2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterExpr {
    Eq(FieldId, CanonicalValue),
    Ne(FieldId, CanonicalValue),
    In(FieldId, Vec<CanonicalValue>),
    Range { field: FieldId, low: Option<(CanonicalValue, bool)>, high: Option<(CanonicalValue, bool)> },
    StartsWith(FieldId, String),
    Contains(FieldId, String),
    Regex(FieldId, String),
    IsNull(FieldId),
    IsPresent(FieldId),
    HasLink { rel_kinds: Vec<RelKindId>, dir: EdgeDir, target: Option<EntityKey> },
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
}

/// Hybrid score fusion method.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FuseMethod {
    /// Default scale-invariant Reciprocal Rank Fusion: `Σ weight_i / (k + rank_i)` (default `k = 60.0`).
    Rrf { k: f64 },
    /// Min-max `[0, 1]` normalization per signal before weighted sum (guards `max == min`).
    LinearNormalized,
}

impl Default for FuseMethod {
    fn default() -> Self {
        Self::Rrf { k: 60.0 }
    }
}

/// Scoring / ranking signal (Operator #3).
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreSpec {
    Bm25f { index_id: IndexId, query: String, require_all: bool, phrase: Option<Vec<String>>, top_k: Option<usize> },
    VectorNear { index_id: IndexId, vector: Vec<f32>, top_k: usize },
    VectorLike { index_id: IndexId, seed: EntityKey, top_k: usize },
    RankIndex { index_id: IndexId },
    FuzzyLabel { field: FieldId, query: String, max_distance: u8 },
    RecencyDecay { field: FieldId, now_ms: i64, half_life_ms: f64 },
    Fuse { inputs: Vec<(ScoreSpec, f64)>, method: FuseMethod },
}

/// Graph traversal step (Operator #4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraverseSpec {
    Hops { rel_kinds: Vec<RelKindId>, dir: EdgeDir, min_hops: u32, max_hops: u32 },
    ShortestPath { rel_kinds: Vec<RelKindId>, dir: EdgeDir, target: EntityKey, max_hops: u32 },
}

/// Deterministic graph analytics step (Operator #5).
#[derive(Debug, Clone, PartialEq)]
pub enum GraphAnalyzeSpec {
    Ppr {
        rel_kinds: Vec<RelKindId>,
        weighted: bool,
        seeds: Vec<(EntityKey, f64)>,
        damping: f64,
        iterations: u32,
        tolerance: f64,
    },
    WeakComponents {
        rel_kinds: Vec<RelKindId>,
    },
    Scc {
        rel_kinds: Vec<RelKindId>,
    },
    TopoSort {
        rel_kinds: Vec<RelKindId>,
    },
    Leiden {
        rel_kinds: Vec<RelKindId>,
        weighted: bool,
        resolution: f64,
        seed: u64,
        iterations: u32,
    },
    Jaccard {
        rel_kinds: Vec<RelKindId>,
        dir: EdgeDir,
        seed: EntityKey,
        top_k: usize,
    },
    AdamicAdar {
        rel_kinds: Vec<RelKindId>,
        dir: EdgeDir,
        seed: EntityKey,
        top_k: usize,
    },
}

/// Ordering criterion for `OrderSlice` (Operator #7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderSpec {
    EntityKey(OrderDir),
    Score(OrderDir),
    Field { field: FieldId, dir: OrderDir, locale: Option<String> },
}

/// Keyset cursor for deterministic pagination.
#[derive(Debug, Clone, PartialEq)]
pub enum Cursor {
    EntityKey(EntityKey),
    Score { score: f64, key: EntityKey },
    Field { value: CanonicalValue, key: EntityKey },
}

/// Terminal output projection / aggregation (Operator #8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSpec {
    Ids,
    Rows { fields: Option<FieldMask> },
    Hits,
    Count,
    Exists,
    Sum(FieldId),
    MinMax(FieldId),
    GroupBy { field: FieldId, counts_only: bool },
    Degrees { rel_kinds: Vec<RelKindId>, dir: EdgeDir },
}

/// The 8 orthogonal algebraic logical operators.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalNode {
    /// #1 Source
    Source(SourceSpec),
    /// #2 Filter
    Filter { input: Box<LogicalNode>, expr: FilterExpr },
    /// #3 Score
    Score { input: Box<LogicalNode>, spec: ScoreSpec },
    /// #4 Traverse
    Traverse { input: Box<LogicalNode>, spec: TraverseSpec },
    /// #5 GraphAnalyze
    GraphAnalyze { input: Box<LogicalNode>, spec: GraphAnalyzeSpec },
    /// #6 Distinct
    Distinct { input: Box<LogicalNode> },
    /// #7 OrderSlice (`limit` is `u64`, never `f64`!)
    OrderSlice { input: Box<LogicalNode>, order: OrderSpec, after: Option<Cursor>, limit: Option<u64> },
    /// #8 Terminal
    Terminal { input: Box<LogicalNode>, terminal: TerminalSpec },
}

/// Top-level compiled query plan.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalPlan {
    pub root: LogicalNode,
}

impl LogicalPlan {
    pub fn new(root: LogicalNode) -> Self {
        Self { root }
    }
}
