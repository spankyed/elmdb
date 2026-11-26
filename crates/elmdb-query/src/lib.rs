#![forbid(unsafe_code)]

//! Clean-room elmdb v2 algebraic query IR, cost/rule optimizer, shape-normalized LRU plan cache,
//! 256-key batched pull executor with hard resource/cancellation budgets, and deterministic graph analytics.

pub mod cache;
pub mod exec;
pub mod graph;
pub mod ir;
pub mod optimizer;

pub use cache::{CachedStrategy, DEFAULT_PLAN_CACHE_CAP, PlanCache, PlanCacheStats, PlanShapeKey};
pub use exec::{
    BATCH_SIZE, DEFAULT_MAX_HEAP_BYTES, DEFAULT_MAX_TRAVERSAL_VISITED, ExecBudgets, KeyBatch, NoopReadRecorder,
    ProjectedRow, QueryEngine, QueryOutput, QueryReadRecorder, QueryResult, hash_canonical_value,
};
pub use graph::{
    CsrGraph, ExactSum, PprResult, TopoResult, adamic_adar_similar, jaccard_similar, leiden_communities, ppr, scc,
    topo_sort, weak_components,
};
pub use ir::{
    Cursor, EdgeDir, FilterExpr, FuseMethod, GraphAnalyzeSpec, LogicalNode, LogicalPlan, OrderDir, OrderSpec,
    ScoreSpec, SourceSpec, TerminalSpec, TraverseSpec,
};
pub use optimizer::{ExplainPlan, extract_regex_trigrams, leapfrog_intersect};
