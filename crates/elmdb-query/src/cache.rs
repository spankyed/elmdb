//! Shape-normalized internal LRU query plan cache (256–512 entries).
//!
//! - Separates structural plan shape from bound literal values so repeated queries with different
//!   IDs/filter values hit the cache instead of thrashing the LRU.
//! - Retains plan-affecting traits in the shape key (`is_null`, `in_cardinality_bucket`,
//!   `limit_present`, `after_present`, `order_dir`, `field_mask`, `schema_epoch`).
//! - Verifies **full structural shape equality** (`PartialEq + Eq`) on hit (never hash-only!)
//!   and clears/skips entries automatically whenever `schema_epoch` advances.

use std::collections::VecDeque;

use elmdb_engine::{FieldId, IndexId, RelKindId, TypeId};
use elmdb_storage::MetaLock;
use elmdb_wire::CanonicalValue;
use rustc_hash::FxHashMap;

use crate::ir::{
    EdgeDir, FilterExpr, FuseMethod, GraphAnalyzeSpec, LogicalNode, LogicalPlan, OrderDir, OrderSpec, ScoreSpec,
    SourceSpec, TerminalSpec, TraverseSpec,
};

pub const DEFAULT_PLAN_CACHE_CAP: usize = 384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InBucket {
    Zero,
    One,
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceShape {
    Type { type_id: TypeId, include_trashed: bool },
    Types { type_ids: Vec<TypeId>, include_trashed: bool },
    AllEntities { include_trashed: bool },
    LiteralKeys(InBucket),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FilterShape {
    Eq { field: FieldId, is_null: bool, is_indexable: bool },
    Ne { field: FieldId, is_null: bool },
    In { field: FieldId, bucket: InBucket },
    Range { field: FieldId, has_low: bool, low_inc: bool, has_high: bool, high_inc: bool },
    StartsWith(FieldId),
    Contains(FieldId),
    Regex(FieldId),
    IsNull(FieldId),
    IsPresent(FieldId),
    HasLink { rel_kinds: Vec<RelKindId>, dir: EdgeDir, has_target: bool },
    And(Vec<FilterShape>),
    Or(Vec<FilterShape>),
    Not(Box<FilterShape>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScoreShape {
    Bm25f { index_id: IndexId, require_all: bool, prefix: bool, has_phrase: bool, has_top_k: bool },
    VectorNear { index_id: IndexId },
    VectorLike { index_id: IndexId },
    RankIndex { index_id: IndexId },
    FuzzyLabel { field: FieldId, max_distance: u8 },
    RecencyDecay { field: FieldId },
    Fuse { inputs: Vec<ScoreShape>, is_rrf: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TraverseShape {
    Hops { rel_kinds: Vec<RelKindId>, dir: EdgeDir, min_hops: u32, max_hops: u32 },
    ShortestPath { rel_kinds: Vec<RelKindId>, dir: EdgeDir, max_hops: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GraphAnalyzeShape {
    Ppr { rel_kinds: Vec<RelKindId>, weighted: bool },
    WeakComponents { rel_kinds: Vec<RelKindId> },
    Scc { rel_kinds: Vec<RelKindId> },
    TopoSort { rel_kinds: Vec<RelKindId> },
    Leiden { rel_kinds: Vec<RelKindId>, weighted: bool },
    Jaccard { rel_kinds: Vec<RelKindId>, dir: EdgeDir },
    AdamicAdar { rel_kinds: Vec<RelKindId>, dir: EdgeDir },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OrderShape {
    EntityKey(OrderDir),
    Score(OrderDir),
    Field { field: FieldId, dir: OrderDir, locale: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TerminalShape {
    Ids,
    Rows { projected_fields: Option<Vec<u16>> },
    Hits { projected_fields: Option<Vec<u16>>, highlights: bool },
    Count,
    Exists,
    Sum(FieldId),
    MinMax(FieldId),
    GroupBy { field: FieldId, counts_only: bool },
    Degrees { rel_kinds: Vec<RelKindId>, dir: EdgeDir },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeShape {
    Source(SourceShape),
    Filter { input: Box<NodeShape>, expr: FilterShape },
    Score { input: Box<NodeShape>, spec: ScoreShape },
    Traverse { input: Box<NodeShape>, spec: TraverseShape },
    GraphAnalyze { input: Box<NodeShape>, spec: GraphAnalyzeShape },
    Distinct { input: Box<NodeShape> },
    OrderSlice { input: Box<NodeShape>, order: OrderShape, after_present: bool, limit_present: bool },
    Terminal { input: Box<NodeShape>, terminal: TerminalShape },
}

/// Full structural plan-cache key including `(schema_epoch, maint_seq)` (zero hash-only collisions!).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanShapeKey {
    pub schema_epoch: u64,
    pub maint_seq: u64,
    pub root: NodeShape,
}

impl PlanShapeKey {
    pub fn from_plan(schema_epoch: u64, maint_seq: u64, plan: &LogicalPlan) -> Self {
        Self { schema_epoch, maint_seq, root: shape_node(&plan.root) }
    }
}

fn bucket_len(n: usize) -> InBucket {
    match n {
        0 => InBucket::Zero,
        1 => InBucket::One,
        _ => InBucket::Many,
    }
}

fn shape_filter(expr: &FilterExpr) -> FilterShape {
    match expr {
        FilterExpr::Eq(field, val) => FilterShape::Eq {
            field: *field,
            is_null: matches!(val, CanonicalValue::Null),
            is_indexable: matches!(
                elmdb_projections::value::encode_compound_values(&[val]),
                elmdb_projections::value::ValueKeyClass::Indexed(_)
            ),
        },
        FilterExpr::Ne(field, val) => FilterShape::Ne { field: *field, is_null: matches!(val, CanonicalValue::Null) },
        FilterExpr::In(field, vals) => FilterShape::In { field: *field, bucket: bucket_len(vals.len()) },
        FilterExpr::Range { field, low, high } => FilterShape::Range {
            field: *field,
            has_low: low.is_some(),
            low_inc: low.as_ref().is_some_and(|(_, i)| *i),
            has_high: high.is_some(),
            high_inc: high.as_ref().is_some_and(|(_, i)| *i),
        },
        FilterExpr::StartsWith(f, _) => FilterShape::StartsWith(*f),
        FilterExpr::Contains(f, _) => FilterShape::Contains(*f),
        FilterExpr::Regex(f, _) => FilterShape::Regex(*f),
        FilterExpr::IsNull(f) => FilterShape::IsNull(*f),
        FilterExpr::IsPresent(f) => FilterShape::IsPresent(*f),
        FilterExpr::HasLink { rel_kinds, dir, target } => {
            FilterShape::HasLink { rel_kinds: rel_kinds.clone(), dir: *dir, has_target: target.is_some() }
        }
        FilterExpr::And(xs) => FilterShape::And(xs.iter().map(shape_filter).collect()),
        FilterExpr::Or(xs) => FilterShape::Or(xs.iter().map(shape_filter).collect()),
        FilterExpr::Not(inner) => FilterShape::Not(Box::new(shape_filter(inner))),
    }
}

fn shape_score(spec: &ScoreSpec) -> ScoreShape {
    match spec {
        ScoreSpec::Bm25f { index_id, require_all, prefix, phrase, top_k, .. } => ScoreShape::Bm25f {
            index_id: *index_id,
            require_all: *require_all,
            prefix: *prefix,
            has_phrase: phrase.is_some(),
            has_top_k: top_k.is_some(),
        },
        ScoreSpec::VectorNear { index_id, .. } => ScoreShape::VectorNear { index_id: *index_id },
        ScoreSpec::VectorLike { index_id, .. } => ScoreShape::VectorLike { index_id: *index_id },
        ScoreSpec::RankIndex { index_id } => ScoreShape::RankIndex { index_id: *index_id },
        ScoreSpec::FuzzyLabel { field, max_distance, .. } => {
            ScoreShape::FuzzyLabel { field: *field, max_distance: *max_distance }
        }
        ScoreSpec::RecencyDecay { field, .. } => ScoreShape::RecencyDecay { field: *field },
        ScoreSpec::Fuse { inputs, method } => ScoreShape::Fuse {
            inputs: inputs.iter().map(|(s, _)| shape_score(s)).collect(),
            is_rrf: matches!(method, FuseMethod::Rrf { .. }),
        },
    }
}

fn shape_node(node: &LogicalNode) -> NodeShape {
    match node {
        LogicalNode::Source(src) => NodeShape::Source(match src {
            SourceSpec::Type { type_id, include_trashed } => {
                SourceShape::Type { type_id: *type_id, include_trashed: *include_trashed }
            }
            SourceSpec::Types { type_ids, include_trashed } => {
                SourceShape::Types { type_ids: type_ids.clone(), include_trashed: *include_trashed }
            }
            SourceSpec::AllEntities { include_trashed } => {
                SourceShape::AllEntities { include_trashed: *include_trashed }
            }
            SourceSpec::LiteralKeys(ks) => SourceShape::LiteralKeys(bucket_len(ks.len())),
        }),
        LogicalNode::Filter { input, expr } => {
            NodeShape::Filter { input: Box::new(shape_node(input)), expr: shape_filter(expr) }
        }
        LogicalNode::Score { input, spec } => {
            NodeShape::Score { input: Box::new(shape_node(input)), spec: shape_score(spec) }
        }
        LogicalNode::Traverse { input, spec } => NodeShape::Traverse {
            input: Box::new(shape_node(input)),
            spec: match spec {
                TraverseSpec::Hops { rel_kinds, dir, min_hops, max_hops } => TraverseShape::Hops {
                    rel_kinds: rel_kinds.clone(),
                    dir: *dir,
                    min_hops: *min_hops,
                    max_hops: *max_hops,
                },
                TraverseSpec::ShortestPath { rel_kinds, dir, max_hops, .. } => {
                    TraverseShape::ShortestPath { rel_kinds: rel_kinds.clone(), dir: *dir, max_hops: *max_hops }
                }
            },
        },
        LogicalNode::GraphAnalyze { input, spec } => NodeShape::GraphAnalyze {
            input: Box::new(shape_node(input)),
            spec: match spec {
                GraphAnalyzeSpec::Ppr { rel_kinds, weighted, .. } => {
                    GraphAnalyzeShape::Ppr { rel_kinds: rel_kinds.clone(), weighted: *weighted }
                }
                GraphAnalyzeSpec::WeakComponents { rel_kinds } => {
                    GraphAnalyzeShape::WeakComponents { rel_kinds: rel_kinds.clone() }
                }
                GraphAnalyzeSpec::Scc { rel_kinds } => GraphAnalyzeShape::Scc { rel_kinds: rel_kinds.clone() },
                GraphAnalyzeSpec::TopoSort { rel_kinds } => {
                    GraphAnalyzeShape::TopoSort { rel_kinds: rel_kinds.clone() }
                }
                GraphAnalyzeSpec::Leiden { rel_kinds, weighted, .. } => {
                    GraphAnalyzeShape::Leiden { rel_kinds: rel_kinds.clone(), weighted: *weighted }
                }
                GraphAnalyzeSpec::Jaccard { rel_kinds, dir, .. } => {
                    GraphAnalyzeShape::Jaccard { rel_kinds: rel_kinds.clone(), dir: *dir }
                }
                GraphAnalyzeSpec::AdamicAdar { rel_kinds, dir, .. } => {
                    GraphAnalyzeShape::AdamicAdar { rel_kinds: rel_kinds.clone(), dir: *dir }
                }
            },
        },
        LogicalNode::Distinct { input } => NodeShape::Distinct { input: Box::new(shape_node(input)) },
        LogicalNode::OrderSlice { input, order, after, limit } => NodeShape::OrderSlice {
            input: Box::new(shape_node(input)),
            order: match order {
                OrderSpec::EntityKey(d) => OrderShape::EntityKey(*d),
                OrderSpec::Score(d) => OrderShape::Score(*d),
                OrderSpec::Field { field, dir, locale } => {
                    OrderShape::Field { field: *field, dir: *dir, locale: locale.clone() }
                }
            },
            after_present: after.is_some(),
            limit_present: limit.is_some(),
        },
        LogicalNode::Terminal { input, terminal } => NodeShape::Terminal {
            input: Box::new(shape_node(input)),
            terminal: match terminal {
                TerminalSpec::Ids => TerminalShape::Ids,
                TerminalSpec::Rows { fields } => {
                    let projected = fields.as_ref().map(|m| m.iter().into_iter().map(|fid| fid.0).collect());
                    TerminalShape::Rows { projected_fields: projected }
                }
                TerminalSpec::Hits { fields, highlights } => {
                    let projected = fields.as_ref().map(|m| m.iter().into_iter().map(|fid| fid.0).collect());
                    TerminalShape::Hits { projected_fields: projected, highlights: *highlights }
                }
                TerminalSpec::Count => TerminalShape::Count,
                TerminalSpec::Exists => TerminalShape::Exists,
                TerminalSpec::Sum(f) => TerminalShape::Sum(*f),
                TerminalSpec::MinMax(f) => TerminalShape::MinMax(*f),
                TerminalSpec::GroupBy { field, counts_only } => {
                    TerminalShape::GroupBy { field: *field, counts_only: *counts_only }
                }
                TerminalSpec::Degrees { rel_kinds, dir } => {
                    TerminalShape::Degrees { rel_kinds: rel_kinds.clone(), dir: *dir }
                }
            },
        },
    }
}

/// Cached optimizer decision (access strategy independent of literal values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedStrategy {
    pub access_label: String,
    pub chosen_index: Option<IndexId>,
    pub use_count_pushdown: bool,
    pub use_index_order: Option<IndexId>,
    pub use_trigram_index: Option<IndexId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
    pub schema_epoch: u64,
    pub maint_seq: u64,
}

struct CacheInner {
    capacity: usize,
    epoch: u64,
    maint_seq: u64,
    map: FxHashMap<PlanShapeKey, CachedStrategy>,
    lru: VecDeque<PlanShapeKey>,
    hits: u64,
    misses: u64,
}

/// Thread-safe bounded internal LRU query plan cache (automatically cleared on `(schema_epoch, maint_seq)` advance).
pub struct PlanCache {
    inner: MetaLock<CacheInner>,
}

impl Default for PlanCache {
    fn default() -> Self {
        Self::new(DEFAULT_PLAN_CACHE_CAP)
    }
}

impl PlanCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: MetaLock::new(CacheInner {
                capacity: capacity.clamp(16, 4096),
                epoch: 0,
                maint_seq: 0,
                map: FxHashMap::default(),
                lru: VecDeque::new(),
                hits: 0,
                misses: 0,
            }),
        }
    }

    pub fn get(&self, key: &PlanShapeKey) -> Option<CachedStrategy> {
        let mut guard = self.inner.lock();
        let key_ver = (key.schema_epoch, key.maint_seq);
        let cur_ver = (guard.epoch, guard.maint_seq);
        if key_ver > cur_ver {
            guard.map.clear();
            guard.lru.clear();
            guard.epoch = key.schema_epoch;
            guard.maint_seq = key.maint_seq;
        }
        if key_ver != (guard.epoch, guard.maint_seq) {
            guard.misses += 1;
            return None;
        }
        if let Some(val) = guard.map.get(key).cloned() {
            guard.hits += 1;
            // Refresh LRU position.
            if let Some(pos) = guard.lru.iter().position(|k| k == key) {
                guard.lru.remove(pos);
            }
            guard.lru.push_back(key.clone());
            return Some(val);
        }
        guard.misses += 1;
        None
    }

    pub fn put(&self, key: PlanShapeKey, strategy: CachedStrategy) {
        let mut guard = self.inner.lock();
        let key_ver = (key.schema_epoch, key.maint_seq);
        let cur_ver = (guard.epoch, guard.maint_seq);
        if key_ver < cur_ver {
            return;
        }
        if key_ver > cur_ver {
            guard.map.clear();
            guard.lru.clear();
            guard.epoch = key.schema_epoch;
            guard.maint_seq = key.maint_seq;
        }
        if guard.map.contains_key(&key) {
            if let Some(pos) = guard.lru.iter().position(|k| k == &key) {
                guard.lru.remove(pos);
            }
        } else if guard.map.len() >= guard.capacity
            && let Some(evicted) = guard.lru.pop_front()
        {
            guard.map.remove(&evicted);
        }
        guard.lru.push_back(key.clone());
        guard.map.insert(key, strategy);
    }

    pub fn clear(&self) {
        let mut guard = self.inner.lock();
        guard.map.clear();
        guard.lru.clear();
    }

    pub fn stats(&self) -> PlanCacheStats {
        let guard = self.inner.lock();
        PlanCacheStats {
            hits: guard.hits,
            misses: guard.misses,
            entries: guard.map.len(),
            schema_epoch: guard.epoch,
            maint_seq: guard.maint_seq,
        }
    }
}
