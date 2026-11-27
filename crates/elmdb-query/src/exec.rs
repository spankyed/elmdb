//! Vectorized 256-key batched pull executor (`KeyBatch`), `ExecCtx` hard resource & cancellation
//! budgets, `QueryReadRecorder` seam for Phase 3b live-query invalidation, and `QueryEngine`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};

use elmdb_engine::{
    CompiledSchema, Engine, EntityKey, FieldId, FieldMask, IndexId, IndexKind, PackedRow, RelKindId, SystemDbs, TypeId,
    read_packed_row, scan_adj_in_raw, scan_adj_out_raw, scan_type_keys_in_range,
};
use elmdb_projections::{
    CollatorCache, order, rank, text, trigram,
    value::{self, ValueKeyClass},
};
use elmdb_storage::{Clock, DbFlags, KvCursor, KvRead, KvStore, SubDbHandle, Vfs};
use elmdb_vec::{SearchNearOpts, search_like, search_near};
use elmdb_wire::{CanonicalValue, ElmdbError, ErrorCode, Result, compare_numeric};
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder};
use regex::Regex;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::cache::{PlanCache, PlanCacheStats};
use crate::graph::{
    CsrGraph, ExactSum, GraphBuildOpts, PprResult, TopoResult, adamic_adar_similar_tracked, jaccard_similar_tracked,
    leiden_communities, ppr_cancellable, scc, topo_sort, weak_components,
};
use crate::ir::{
    Cursor, EdgeDir, FilterExpr, FuseMethod, GraphAnalyzeSpec, LogicalNode, LogicalPlan, OrderDir, OrderSpec,
    ScoreSpec, SourceSpec, TerminalSpec, TraverseSpec,
};
use crate::optimizer::{
    ExplainPlan, extract_regex_trigrams, find_ready_order_index, find_ready_trigram_index, find_ready_value_index,
    flatten_and, is_index_ready, leapfrog_intersect, optimize_plan,
};

/// L1-cache-resident working batch (`256 * 8 B = 2 KiB` keys + `256 * 8 B = 2 KiB` scores).
pub const BATCH_SIZE: usize = 256;

/// Default hard memory ceiling for sorts, hashes, and BFS frontiers (`16 MiB`).
pub const DEFAULT_MAX_HEAP_BYTES: usize = 16 * 1024 * 1024;
/// Default hard traversal visited-node ceiling.
pub const DEFAULT_MAX_TRAVERSAL_VISITED: usize = 1_000_000;

#[derive(Debug, Clone)]
pub struct KeyBatch {
    pub keys: [EntityKey; BATCH_SIZE],
    pub scores: [f64; BATCH_SIZE],
    pub len: u16,
}

impl Default for KeyBatch {
    fn default() -> Self {
        Self { keys: [EntityKey(0); BATCH_SIZE], scores: [0.0; BATCH_SIZE], len: 0 }
    }
}

impl KeyBatch {
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    pub fn push(&mut self, key: EntityKey, score: f64) -> bool {
        let idx = self.len as usize;
        if idx >= BATCH_SIZE {
            return false;
        }
        self.keys[idx] = key;
        self.scores[idx] = score;
        self.len += 1;
        true
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Hard execution resource and cancellation budgets enforced at every 256-key batch boundary.
#[derive(Debug, Clone, Copy)]
pub struct ExecBudgets<'a> {
    pub max_heap_bytes: usize,
    pub max_traversal_visited: usize,
    pub cancel: Option<&'a AtomicBool>,
}

impl Default for ExecBudgets<'_> {
    fn default() -> Self {
        Self {
            max_heap_bytes: DEFAULT_MAX_HEAP_BYTES,
            max_traversal_visited: DEFAULT_MAX_TRAVERSAL_VISITED,
            cancel: None,
        }
    }
}

/// Read-dependency recording seam implemented by `elmdb-invalidation` in Phase 3b!
pub trait QueryReadRecorder {
    fn record_point_entity(&mut self, _key: EntityKey, _fields: &FieldMask) {}
    /// Records a point-entity read covering ALL current and future fields (e.g. `Rows { fields: None }`).
    fn record_point_entity_all_fields(&mut self, _key: EntityKey) {}
    fn record_indexed_eq(&mut self, _index_id: IndexId, _value_hash: u64) {}
    fn record_edge_adj(&mut self, _entity: EntityKey, _rel_kinds: &[RelKindId], _dir: EdgeDir) {}
    fn record_type_scan(&mut self, _type_id: TypeId) {}
    fn record_all_entities(&mut self) {}
    fn record_subdb_range(&mut self, _slot: u16, _low: &[u8], _high: &[u8]) {}
}

/// No-op read recorder used for untracked one-shot queries.
#[derive(Debug, Default)]
pub struct NoopReadRecorder;
impl QueryReadRecorder for NoopReadRecorder {}

/// Single UTF-16 code-unit highlight span `(element_index, utf16_start, utf16_end)`.
pub type HighlightSpan = (u32, usize, usize);
/// Per-field UTF-16 search highlight spans map.
pub type SearchHighlights = BTreeMap<String, Vec<HighlightSpan>>;

/// Single scored hit returned by `TerminalSpec::Hits` (optionally carrying a projected row and lazy UTF-16 highlights).
#[derive(Debug, Clone, PartialEq)]
pub struct HitOutput {
    pub key: EntityKey,
    pub score: f64,
    pub row: Option<ProjectedRow>,
    pub highlights: Option<SearchHighlights>,
}

/// Typed result produced by executing a `LogicalPlan`.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Ids(Vec<EntityKey>),
    Rows(Vec<ProjectedRow>),
    Hits(Vec<HitOutput>),
    Count(u64),
    Exists(bool),
    Sum(f64),
    MinMax { min: Option<CanonicalValue>, max: Option<CanonicalValue> },
    GroupCounts(Vec<(CanonicalValue, u64)>),
    Groups(Vec<(CanonicalValue, Vec<EntityKey>)>),
    Degrees(Vec<(EntityKey, u64)>),
    GraphGroups(Vec<Vec<EntityKey>>),
    GraphTopo(TopoResult),
    GraphPpr(PprResult),
}

/// Projected row returned by `TerminalSpec::Rows`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedRow {
    pub key: EntityKey,
    pub id: String,
    pub type_id: TypeId,
    pub trashed: bool,
    pub fields: BTreeMap<FieldId, CanonicalValue>,
}

/// Complete execution output carrying both the typed `QueryResult` and `ExplainPlan`.
#[derive(Debug, Clone)]
pub struct QueryOutput {
    pub result: QueryResult,
    pub explain: ExplainPlan,
}

/// Batched pull operator trait (L1-resident 256-key batches with `demand` clamping!).
trait PhysicalOp {
    fn next_batch<R: KvRead>(&mut self, ctx: &mut ExecCtx<'_, R>, demand: u16, out: &mut KeyBatch) -> Result<bool>;
}

struct VecStreamOp {
    items: Vec<(EntityKey, f64)>,
    pos: usize,
}

impl VecStreamOp {
    fn new(items: Vec<(EntityKey, f64)>) -> Self {
        Self { items, pos: 0 }
    }
}

impl PhysicalOp for VecStreamOp {
    fn next_batch<R: KvRead>(&mut self, ctx: &mut ExecCtx<'_, R>, demand: u16, out: &mut KeyBatch) -> Result<bool> {
        ctx.check_cancel()?;
        out.clear();
        let cap = (demand as usize).clamp(1, BATCH_SIZE);
        while self.pos < self.items.len() && (out.len as usize) < cap {
            let (k, s) = self.items[self.pos];
            self.pos += 1;
            out.push(k, s);
        }
        if out.is_empty() {
            Ok(false)
        } else {
            ctx.explain.batches_pulled += 1;
            Ok(true)
        }
    }
}

/// Execution context carrying MVCC RO or active RW snapshot, pinned schema, budgets, read recorder, and `EXPLAIN`.
struct ExecCtx<'a, R: KvRead> {
    txn: &'a R,
    schema: &'a CompiledSchema,
    dbs: SystemDbs,
    collators: &'a CollatorCache,
    budgets: ExecBudgets<'a>,
    recorder: &'a mut dyn QueryReadRecorder,
    explain: ExplainPlan,
    heap_used: usize,
    row_cache: FxHashMap<EntityKey, PackedRow>,
    regex_cache: FxHashMap<String, Regex>,
}

impl<'a, R: KvRead> ExecCtx<'a, R> {
    #[inline]
    fn check_cancel(&self) -> Result<()> {
        if let Some(flag) = self.budgets.cancel
            && flag.load(AtomicOrd::Relaxed)
        {
            return Err(ElmdbError::new(ErrorCode::Cancelled, "query cancelled"));
        }
        Ok(())
    }

    #[inline]
    fn charge_heap(&mut self, bytes: usize) -> Result<()> {
        self.heap_used = self.heap_used.saturating_add(bytes);
        if self.heap_used > self.budgets.max_heap_bytes {
            return Err(ElmdbError::new(
                ErrorCode::ResourceLimit,
                format!("query exceeded heap budget ({} B)", self.budgets.max_heap_bytes),
            ));
        }
        Ok(())
    }

    #[inline]
    fn remaining_heap(&self) -> usize {
        self.budgets.max_heap_bytes.saturating_sub(self.heap_used)
    }

    fn open_index_ro(&self, index_id: IndexId) -> Result<Option<SubDbHandle>> {
        let def = self
            .schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        let handle = self.txn.open_subdb_existing(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
        Ok(handle)
    }

    fn load_row(&mut self, key: EntityKey) -> Result<Option<PackedRow>> {
        if let Some(cached) = self.row_cache.get(&key) {
            return Ok(Some(cached.clone()));
        }
        let Some(row) = read_packed_row(self.txn, &self.dbs, key)? else {
            return Ok(None);
        };
        self.explain.rows_scanned += 1;
        // Keep bounded row cache only when the cached row fits comfortably within remaining heap budget.
        let approx = estimate_row_bytes(&row);
        if self.row_cache.len() < 4096
            && approx <= 16_384
            && self.heap_used.saturating_add(approx) <= self.budgets.max_heap_bytes
        {
            self.heap_used = self.heap_used.saturating_add(approx);
            self.row_cache.insert(key, row.clone());
        }
        Ok(Some(row))
    }
}

fn estimate_value_bytes(v: &CanonicalValue) -> usize {
    match v {
        CanonicalValue::Null | CanonicalValue::Bool(_) | CanonicalValue::Int(_) | CanonicalValue::Float(_) => 16,
        CanonicalValue::Timestamp { .. } | CanonicalValue::EntityRef(_) => 24,
        CanonicalValue::String(s) | CanonicalValue::DanglingRef(s) => 24 + s.len(),
        CanonicalValue::Bytes(b) => 24 + b.len(),
        CanonicalValue::Array(xs) => 24 + xs.iter().map(estimate_value_bytes).sum::<usize>(),
        CanonicalValue::Object(map) => {
            24 + map.iter().map(|(k, val)| 24 + k.len() + estimate_value_bytes(val)).sum::<usize>()
        }
    }
}

fn estimate_row_bytes(row: &PackedRow) -> usize {
    64 + row.header.ext_id.len() + row.fields.values().map(|v| 16 + estimate_value_bytes(v)).sum::<usize>()
}

fn precompile_plan_regexes(node: &LogicalNode, cache: &mut FxHashMap<String, Regex>) -> Result<()> {
    match node {
        LogicalNode::Filter { input, expr } => {
            precompile_filter_regexes(expr, cache)?;
            precompile_plan_regexes(input, cache)
        }
        LogicalNode::Score { input, .. }
        | LogicalNode::Traverse { input, .. }
        | LogicalNode::GraphAnalyze { input, .. }
        | LogicalNode::Distinct { input }
        | LogicalNode::OrderSlice { input, .. }
        | LogicalNode::Terminal { input, .. } => precompile_plan_regexes(input, cache),
        LogicalNode::Source(_) => Ok(()),
    }
}

fn precompile_filter_regexes(expr: &FilterExpr, cache: &mut FxHashMap<String, Regex>) -> Result<()> {
    match expr {
        FilterExpr::Regex(_, pattern) => {
            if !cache.contains_key(pattern) {
                let re = Regex::new(pattern).map_err(|e| {
                    ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid regex '{pattern}': {e}"))
                })?;
                cache.insert(pattern.clone(), re);
            }
            Ok(())
        }
        FilterExpr::And(parts) | FilterExpr::Or(parts) => {
            for p in parts {
                precompile_filter_regexes(p, cache)?;
            }
            Ok(())
        }
        FilterExpr::Not(inner) => precompile_filter_regexes(inner, cache),
        _ => Ok(()),
    }
}

/// Top-level query engine holding the shape-normalized internal LRU plan cache and collator cache.
#[derive(Default)]
pub struct QueryEngine {
    plan_cache: PlanCache,
    collators: CollatorCache,
}

impl QueryEngine {
    pub fn new(plan_cache_capacity: usize) -> Self {
        Self { plan_cache: PlanCache::new(plan_cache_capacity), collators: CollatorCache::default() }
    }

    pub fn cache_stats(&self) -> PlanCacheStats {
        self.plan_cache.stats()
    }

    pub fn clear_cache(&self) {
        self.plan_cache.clear();
    }

    /// Executes `plan` on a fresh MVCC snapshot pinned to its matching `CompiledSchema`.
    pub fn execute<S: KvStore, V: Vfs, C: Clock>(
        &self,
        engine: &Engine<S, V, C>,
        plan: &LogicalPlan,
        budgets: ExecBudgets<'_>,
    ) -> Result<QueryOutput> {
        let mut noop = NoopReadRecorder;
        self.execute_tracked(engine, plan, budgets, &mut noop)
    }

    /// Executes `plan` while recording fine-grained read dependencies into `recorder` (Phase 3b seam!).
    pub fn execute_tracked<S: KvStore, V: Vfs, C: Clock>(
        &self,
        engine: &Engine<S, V, C>,
        plan: &LogicalPlan,
        budgets: ExecBudgets<'_>,
        recorder: &mut dyn QueryReadRecorder,
    ) -> Result<QueryOutput> {
        let (txn, schema) = engine.begin_read_pinned()?;
        self.execute_in_txn(engine, &txn, &schema, plan, budgets, recorder)
    }

    /// Executes `plan` inside an already-open MVCC `RoTxn` or active `GatedWriteTxn` (RYOW).
    pub fn execute_in_txn<S: KvStore, V: Vfs, C: Clock, R: KvRead>(
        &self,
        engine: &Engine<S, V, C>,
        txn: &R,
        schema: &CompiledSchema,
        plan: &LogicalPlan,
        budgets: ExecBudgets<'_>,
        recorder: &mut dyn QueryReadRecorder,
    ) -> Result<QueryOutput> {
        let dbs = engine.dbs();
        let (strategy, cache_hit) = optimize_plan(txn, &dbs, schema, plan, Some(&self.plan_cache))?;
        let mut explain = ExplainPlan {
            access_path: strategy.access_label.clone(),
            count_pushdown: strategy.use_count_pushdown,
            plan_cache_hit: cache_hit,
            ..Default::default()
        };
        if let Some(id) = strategy.chosen_index
            && let Some(def) = schema.index_by_id(id)
        {
            explain.chosen_indexes.push(def.name.clone());
        }
        if let Some(id) = strategy.use_trigram_index
            && let Some(def) = schema.index_by_id(id)
        {
            explain.chosen_indexes.push(def.name.clone());
        }
        if let Some(id) = strategy.use_index_order
            && let Some(def) = schema.index_by_id(id)
        {
            explain.order_pushdown = Some(def.name.clone());
        }

        let mut regex_cache = FxHashMap::default();
        precompile_plan_regexes(&plan.root, &mut regex_cache)?;

        let mut ctx = ExecCtx {
            txn,
            schema,
            dbs,
            collators: &self.collators,
            budgets,
            recorder,
            explain,
            heap_used: 0,
            row_cache: FxHashMap::default(),
            regex_cache,
        };

        // Fast path: O(1) exact Count pushdown without reading a single row!
        // Defense in depth: verify single-field Value index AND `ValueKeyClass::Indexed`.
        if strategy.use_count_pushdown
            && let LogicalNode::Terminal { input, terminal: TerminalSpec::Count } = &plan.root
            && let LogicalNode::Filter { expr: FilterExpr::Eq(_, val), .. } = input.as_ref()
            && matches!(value::encode_compound_values(&[val]), ValueKeyClass::Indexed(_))
            && let Some(idx_id) = strategy.chosen_index
            && let Some(def) = schema.index_by_id(idx_id)
            && matches!(&def.kind, IndexKind::Value { fields, .. } if fields.len() == 1)
            && let Some(db) = ctx.open_index_ro(idx_id)?
        {
            let h = hash_canonical_value(val);
            ctx.recorder.record_indexed_eq(idx_id, h);
            let cnt = value::count_eq(ctx.txn, db, &[val])?;
            ctx.explain.rows_returned = 1;
            return Ok(QueryOutput { result: QueryResult::Count(cnt), explain: ctx.explain });
        }

        let result = eval_root(&mut ctx, &plan.root)?;
        Ok(QueryOutput { result, explain: ctx.explain })
    }
}

// ---------------------------------------------------------------------------
// Recursive plan evaluator (streaming 256-key batches with demand clamping)
// ---------------------------------------------------------------------------

fn eval_root<R: KvRead>(ctx: &mut ExecCtx<'_, R>, node: &LogicalNode) -> Result<QueryResult> {
    match node {
        LogicalNode::GraphAnalyze { input, spec } => eval_graph_analyze(ctx, input, spec),
        LogicalNode::Terminal { input, terminal } => {
            // If input is GraphAnalyze directly under Terminal, let GraphAnalyze produce its structured result
            // unless terminal is Ids/Count/Exists/Hits!
            if let LogicalNode::GraphAnalyze { input: g_in, spec } = input.as_ref() {
                let gres = eval_graph_analyze(ctx, g_in, spec)?;
                return project_graph_terminal(ctx, gres, terminal);
            }
            eval_terminal(ctx, input, terminal)
        }
        other => {
            let stream = eval_stream(ctx, other, None)?;
            let keys: Vec<EntityKey> =
                collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?.into_iter().map(|(k, _)| k).collect();
            ctx.explain.rows_returned = keys.len() as u64;
            Ok(QueryResult::Ids(keys))
        }
    }
}

fn project_graph_terminal<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    gres: QueryResult,
    terminal: &TerminalSpec,
) -> Result<QueryResult> {
    match (gres, terminal) {
        (QueryResult::GraphPpr(ppr), TerminalSpec::Hits { fields, highlights }) => {
            Ok(QueryResult::Hits(materialize_hits(ctx, ppr.scores, fields.as_ref(), *highlights, &[])?))
        }
        (QueryResult::GraphPpr(ppr), TerminalSpec::Ids) => {
            Ok(QueryResult::Ids(ppr.scores.into_iter().map(|(k, _)| k).collect()))
        }
        (QueryResult::GraphPpr(ppr), TerminalSpec::Count) => Ok(QueryResult::Count(ppr.scores.len() as u64)),
        (QueryResult::GraphPpr(ppr), TerminalSpec::Exists) => Ok(QueryResult::Exists(!ppr.scores.is_empty())),
        (QueryResult::GraphTopo(topo), TerminalSpec::Ids) => Ok(QueryResult::Ids(topo.order)),
        (QueryResult::GraphTopo(topo), TerminalSpec::Count) => Ok(QueryResult::Count(topo.order.len() as u64)),
        (QueryResult::GraphTopo(topo), TerminalSpec::Exists) => Ok(QueryResult::Exists(!topo.order.is_empty())),
        (QueryResult::GraphGroups(groups), TerminalSpec::Count) => Ok(QueryResult::Count(groups.len() as u64)),
        (QueryResult::GraphGroups(groups), TerminalSpec::Exists) => Ok(QueryResult::Exists(!groups.is_empty())),
        (QueryResult::Hits(h), TerminalSpec::Hits { fields, highlights }) => {
            if fields.is_none() && !highlights {
                Ok(QueryResult::Hits(h))
            } else {
                let pairs: Vec<(EntityKey, f64)> = h.into_iter().map(|x| (x.key, x.score)).collect();
                Ok(QueryResult::Hits(materialize_hits(ctx, pairs, fields.as_ref(), *highlights, &[])?))
            }
        }
        (QueryResult::Hits(h), TerminalSpec::Ids) => Ok(QueryResult::Ids(h.into_iter().map(|x| x.key).collect())),
        (QueryResult::Hits(h), TerminalSpec::Count) => Ok(QueryResult::Count(h.len() as u64)),
        (QueryResult::Hits(h), TerminalSpec::Exists) => Ok(QueryResult::Exists(!h.is_empty())),
        (_, term) => Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("terminal {term:?} is not supported directly over GraphAnalyze"),
        )),
    }
}

type Bm25fHighlightSpec<'a> = (IndexId, &'a str, Option<&'a [String]>, bool);

fn collect_bm25f_from_score<'a>(spec: &'a ScoreSpec, out: &mut Vec<Bm25fHighlightSpec<'a>>) {
    match spec {
        ScoreSpec::Bm25f { index_id, query, prefix, phrase, .. } => {
            out.push((*index_id, query.as_str(), phrase.as_deref(), *prefix));
        }
        ScoreSpec::Fuse { inputs, .. } => {
            for (sub, _) in inputs {
                collect_bm25f_from_score(sub, out);
            }
        }
        _ => {}
    }
}

fn collect_bm25f_specs<'a>(node: &'a LogicalNode, out: &mut Vec<Bm25fHighlightSpec<'a>>) {
    match node {
        LogicalNode::Score { input, spec } => {
            collect_bm25f_from_score(spec, out);
            collect_bm25f_specs(input, out);
        }
        LogicalNode::Filter { input, .. }
        | LogicalNode::Traverse { input, .. }
        | LogicalNode::GraphAnalyze { input, .. }
        | LogicalNode::Distinct { input }
        | LogicalNode::OrderSlice { input, .. }
        | LogicalNode::Terminal { input, .. } => collect_bm25f_specs(input, out),
        LogicalNode::Source(_) => {}
    }
}

fn materialize_hits<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    pairs: Vec<(EntityKey, f64)>,
    fields: Option<&FieldMask>,
    highlights: bool,
    bm25_specs: &[Bm25fHighlightSpec<'_>],
) -> Result<Vec<HitOutput>> {
    if fields.is_none() && !highlights {
        return Ok(pairs
            .into_iter()
            .map(|(key, score)| HitOutput { key, score, row: None, highlights: None })
            .collect());
    }

    let mut out = Vec::with_capacity(pairs.len());
    for (ek, score) in pairs {
        ctx.check_cancel()?;
        let row_opt = ctx.load_row(ek)?;

        let proj_row = match fields {
            Some(fm) => {
                ctx.recorder.record_point_entity(ek, fm);
                if let Some(ref row) = row_opt {
                    let mut proj_fields = BTreeMap::new();
                    for (&fid, val) in &row.fields {
                        if fm.contains(fid) {
                            proj_fields.insert(fid, val.clone());
                        }
                    }
                    let row_bytes = 64
                        + row.header.ext_id.len()
                        + proj_fields.values().map(|v| 16 + estimate_value_bytes(v)).sum::<usize>();
                    ctx.charge_heap(row_bytes)?;
                    Some(ProjectedRow {
                        key: ek,
                        id: row.header.ext_id.clone(),
                        type_id: row.header.type_id,
                        trashed: row.header.is_trashed(),
                        fields: proj_fields,
                    })
                } else {
                    None
                }
            }
            None => None,
        };

        let hit_highlights = if highlights {
            let mut hl_map: BTreeMap<String, Vec<(u32, usize, usize)>> = BTreeMap::new();
            for &(index_id, query, phrase, prefix) in bm25_specs {
                let Some(def) = ctx.schema.index_by_id(index_id) else { continue };
                let IndexKind::Text { fields: idx_fields } = &def.kind else { continue };
                let Some(tdef) = ctx.schema.type_by_id(def.type_id) else { continue };

                // Record read dependency on every indexed text field so live `watchHits({ highlights: true })` stays sound!
                let mut text_mask = FieldMask::empty();
                for &(fid, _) in idx_fields {
                    text_mask.insert(fid);
                }
                ctx.recorder.record_point_entity(ek, &text_mask);

                if let Some(ref row) = row_opt
                    && row.header.type_id == def.type_id
                {
                    let field_defs: Vec<(FieldId, &str)> = idx_fields
                        .iter()
                        .filter_map(|&(fid, _)| tdef.field_by_id(fid).map(|f| (fid, f.name.as_str())))
                        .collect();
                    let computed = text::compute_highlights(row, &field_defs, None, query, phrase, prefix);
                    for (fname, spans) in computed {
                        let entry = hl_map.entry(fname).or_default();
                        if entry.is_empty() {
                            *entry = spans;
                        } else {
                            entry.extend(spans);
                            entry.sort_unstable();
                            let mut merged: Vec<(u32, usize, usize)> = Vec::with_capacity(entry.len());
                            for (el, s, e) in entry.drain(..) {
                                if let Some(last) = merged.last_mut()
                                    && last.0 == el
                                    && s <= last.2
                                {
                                    last.2 = last.2.max(e);
                                } else {
                                    merged.push((el, s, e));
                                }
                            }
                            *entry = merged;
                        }
                    }
                }
            }
            let hl_bytes: usize = hl_map.iter().map(|(k, v)| 32 + k.len() + v.len() * 24).sum();
            if hl_bytes > 0 {
                ctx.charge_heap(hl_bytes)?;
            }
            Some(hl_map)
        } else {
            None
        };

        out.push(HitOutput { key: ek, score, row: proj_row, highlights: hit_highlights });
    }
    Ok(out)
}

/// Returns whether the upstream query source explicitly opted into soft-trashed entities.
/// `LiteralKeys` (`query.ids([...])`) defaults to `false` for graph/link/traversal visibility so
/// `hops`, `shortestPath`, `hasLink`, `degrees`, `jaccard`, and `adamicAdar` never leak or
/// traverse through soft-trashed entities unless the caller opted in via `include_trashed: true`.
fn source_includes_trashed(node: &LogicalNode) -> bool {
    match node {
        LogicalNode::Source(SourceSpec::Type { include_trashed, .. })
        | LogicalNode::Source(SourceSpec::Types { include_trashed, .. })
        | LogicalNode::Source(SourceSpec::AllEntities { include_trashed }) => *include_trashed,
        LogicalNode::Source(SourceSpec::LiteralKeys(_)) => false,
        LogicalNode::Filter { input, .. }
        | LogicalNode::Score { input, .. }
        | LogicalNode::Traverse { input, .. }
        | LogicalNode::GraphAnalyze { input, .. }
        | LogicalNode::Distinct { input }
        | LogicalNode::OrderSlice { input, .. }
        | LogicalNode::Terminal { input, .. } => source_includes_trashed(input),
    }
}

fn build_tracked_csr<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    nodes: Vec<EntityKey>,
    rel_kinds: &[RelKindId],
    weighted: bool,
    opts: &GraphBuildOpts<'_>,
) -> Result<CsrGraph> {
    let mut adj_reads = Vec::new();
    let mut ent_reads = Vec::new();
    let g = CsrGraph::build_tracked(
        ctx.txn,
        &ctx.dbs,
        nodes,
        rel_kinds,
        weighted,
        opts,
        &mut |u| adj_reads.push(u),
        &mut |u| ent_reads.push(u),
    )?;
    for u in ent_reads {
        ctx.recorder.record_point_entity(u, &FieldMask::empty());
    }
    for u in adj_reads {
        ctx.recorder.record_edge_adj(u, rel_kinds, EdgeDir::Out);
    }
    Ok(g)
}

fn eval_graph_analyze<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    input: &LogicalNode,
    spec: &GraphAnalyzeSpec,
) -> Result<QueryResult> {
    let stream = eval_stream(ctx, input, None)?;
    let pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;
    let nodes: Vec<EntityKey> = pairs.into_iter().map(|(k, _)| k).collect();
    let opts = GraphBuildOpts {
        max_heap_bytes: ctx.remaining_heap(),
        cancel: ctx.budgets.cancel,
        include_trashed: source_includes_trashed(input),
    };

    match spec {
        GraphAnalyzeSpec::WeakComponents { rel_kinds } => {
            let g = build_tracked_csr(ctx, nodes, rel_kinds, false, &opts)?;
            ctx.check_cancel()?;
            let comps = weak_components(&g);
            ctx.explain.rows_returned = comps.len() as u64;
            Ok(QueryResult::GraphGroups(comps))
        }
        GraphAnalyzeSpec::Scc { rel_kinds } => {
            let g = build_tracked_csr(ctx, nodes, rel_kinds, false, &opts)?;
            ctx.check_cancel()?;
            let comps = scc(&g);
            ctx.explain.rows_returned = comps.len() as u64;
            Ok(QueryResult::GraphGroups(comps))
        }
        GraphAnalyzeSpec::TopoSort { rel_kinds } => {
            let g = build_tracked_csr(ctx, nodes, rel_kinds, false, &opts)?;
            ctx.check_cancel()?;
            let res = topo_sort(&g);
            ctx.explain.rows_returned = res.order.len() as u64;
            Ok(QueryResult::GraphTopo(res))
        }
        GraphAnalyzeSpec::Ppr { rel_kinds, weighted, seeds, damping, iterations, tolerance } => {
            let g = build_tracked_csr(ctx, nodes, rel_kinds, *weighted, &opts)?;
            let res = ppr_cancellable(&g, seeds, *damping, *iterations, *tolerance, ctx.budgets.cancel)?;
            ctx.explain.rows_returned = res.scores.len() as u64;
            Ok(QueryResult::GraphPpr(res))
        }
        GraphAnalyzeSpec::Leiden { rel_kinds, weighted, resolution, seed, iterations } => {
            let g = build_tracked_csr(ctx, nodes, rel_kinds, *weighted, &opts)?;
            ctx.check_cancel()?;
            let groups = leiden_communities(&g, *resolution, *seed, *iterations)?;
            ctx.explain.rows_returned = groups.len() as u64;
            Ok(QueryResult::GraphGroups(groups))
        }
        GraphAnalyzeSpec::Jaccard { rel_kinds, dir, seed, top_k } => {
            let cand: FxHashSet<EntityKey> = nodes.into_iter().collect();
            let empty_mask = FieldMask::empty();
            let mut adj_rec: Vec<(EntityKey, EdgeDir)> = Vec::new();
            let mut ent_rec: Vec<EntityKey> = Vec::new();
            let hits = jaccard_similar_tracked(
                ctx.txn,
                &ctx.dbs,
                Some(&cand),
                rel_kinds,
                *dir,
                *seed,
                *top_k,
                &opts,
                &mut |u, d| adj_rec.push((u, d)),
                &mut |ek| ent_rec.push(ek),
            )?;
            for (u, d) in adj_rec {
                ctx.recorder.record_edge_adj(u, rel_kinds, d);
            }
            for ek in ent_rec {
                ctx.recorder.record_point_entity(ek, &empty_mask);
            }
            ctx.explain.rows_returned = hits.len() as u64;
            Ok(QueryResult::Hits(
                hits.into_iter().map(|(key, score)| HitOutput { key, score, row: None, highlights: None }).collect(),
            ))
        }
        GraphAnalyzeSpec::AdamicAdar { rel_kinds, dir, seed, top_k } => {
            let cand: FxHashSet<EntityKey> = nodes.into_iter().collect();
            let empty_mask = FieldMask::empty();
            let mut adj_rec: Vec<(EntityKey, EdgeDir)> = Vec::new();
            let mut ent_rec: Vec<EntityKey> = Vec::new();
            let hits = adamic_adar_similar_tracked(
                ctx.txn,
                &ctx.dbs,
                Some(&cand),
                rel_kinds,
                *dir,
                *seed,
                *top_k,
                &opts,
                &mut |u, d| adj_rec.push((u, d)),
                &mut |ek| ent_rec.push(ek),
            )?;
            for (u, d) in adj_rec {
                ctx.recorder.record_edge_adj(u, rel_kinds, d);
            }
            for ek in ent_rec {
                ctx.recorder.record_point_entity(ek, &empty_mask);
            }
            ctx.explain.rows_returned = hits.len() as u64;
            Ok(QueryResult::Hits(
                hits.into_iter().map(|(key, score)| HitOutput { key, score, row: None, highlights: None }).collect(),
            ))
        }
    }
}

fn eval_terminal<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    input: &LogicalNode,
    terminal: &TerminalSpec,
) -> Result<QueryResult> {
    match terminal {
        TerminalSpec::Exists => {
            // Demand clamping: pull at most 1 key and stop immediately!
            let mut stream = eval_stream(ctx, input, Some(1))?;
            let mut batch = KeyBatch::default();
            let found = stream.next_batch(ctx, 1, &mut batch)? && !batch.is_empty();
            ctx.explain.rows_returned = u64::from(found);
            Ok(QueryResult::Exists(found))
        }
        TerminalSpec::Count => {
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut count = 0u64;
            while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                count = count.saturating_add(batch.len as u64);
            }
            ctx.explain.rows_returned = 1;
            Ok(QueryResult::Count(count))
        }
        TerminalSpec::Ids => {
            let stream = eval_stream(ctx, input, None)?;
            let pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;
            let ids: Vec<EntityKey> = pairs.into_iter().map(|(k, _)| k).collect();
            ctx.explain.rows_returned = ids.len() as u64;
            Ok(QueryResult::Ids(ids))
        }
        TerminalSpec::Hits { fields, highlights } => {
            let stream = eval_stream(ctx, input, None)?;
            let pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;
            let mut bm25_specs = Vec::new();
            if *highlights {
                collect_bm25f_specs(input, &mut bm25_specs);
            }
            let hits = materialize_hits(ctx, pairs, fields.as_ref(), *highlights, &bm25_specs)?;
            ctx.explain.rows_returned = hits.len() as u64;
            Ok(QueryResult::Hits(hits))
        }
        TerminalSpec::Rows { fields } => {
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut rows = Vec::new();
            while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                for i in 0..batch.len as usize {
                    let ek = batch.keys[i];
                    if let Some(row) = ctx.load_row(ek)? {
                        let mut proj_fields = BTreeMap::new();
                        match fields {
                            Some(fm) => {
                                // Record the FULL requested FieldMask (even for fields currently absent!) so setting
                                // a previously missing field invalidates live queries with zero false negatives.
                                ctx.recorder.record_point_entity(ek, fm);
                                for (&fid, val) in &row.fields {
                                    if fm.contains(fid) {
                                        proj_fields.insert(fid, val.clone());
                                    }
                                }
                            }
                            None => {
                                // Unprojected row depends on ALL current and future fields of `ek`!
                                ctx.recorder.record_point_entity_all_fields(ek);
                                for (&fid, val) in &row.fields {
                                    proj_fields.insert(fid, val.clone());
                                }
                            }
                        }
                        let row_bytes = 64
                            + row.header.ext_id.len()
                            + proj_fields.values().map(|v| 16 + estimate_value_bytes(v)).sum::<usize>();
                        ctx.charge_heap(row_bytes)?;
                        let trashed = row.header.is_trashed();
                        rows.push(ProjectedRow {
                            key: ek,
                            id: row.header.ext_id,
                            type_id: row.header.type_id,
                            trashed,
                            fields: proj_fields,
                        });
                    } else {
                        match fields {
                            Some(fm) => ctx.recorder.record_point_entity(ek, fm),
                            None => ctx.recorder.record_point_entity_all_fields(ek),
                        }
                    }
                }
            }
            ctx.explain.rows_returned = rows.len() as u64;
            Ok(QueryResult::Rows(rows))
        }
        TerminalSpec::Sum(field) => {
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut acc = ExactSum::default();
            let mut fmask = FieldMask::empty();
            fmask.insert(*field);
            while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                for i in 0..batch.len as usize {
                    let ek = batch.keys[i];
                    ctx.recorder.record_point_entity(ek, &fmask);
                    if let Some(row) = ctx.load_row(ek)?
                        && let Some(val) = row.fields.get(field)
                    {
                        let n = match val {
                            CanonicalValue::Int(i) => Some(*i as f64),
                            CanonicalValue::Float(f) if f.is_finite() => Some(*f),
                            _ => None,
                        };
                        if let Some(num) = n
                            && !acc.add(num)
                        {
                            return Err(ElmdbError::new(ErrorCode::ResourceLimit, "sum overflowed finite f64"));
                        }
                    }
                }
            }
            ctx.explain.rows_returned = 1;
            Ok(QueryResult::Sum(acc.total()))
        }
        TerminalSpec::MinMax(field) => {
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut min_v: Option<CanonicalValue> = None;
            let mut max_v: Option<CanonicalValue> = None;
            let mut fmask = FieldMask::empty();
            fmask.insert(*field);
            while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                for i in 0..batch.len as usize {
                    let ek = batch.keys[i];
                    ctx.recorder.record_point_entity(ek, &fmask);
                    if let Some(row) = ctx.load_row(ek)?
                        && let Some(val) = row.fields.get(field)
                        && !matches!(val, CanonicalValue::Null)
                    {
                        if min_v.as_ref().is_none_or(|cur| compare_values_total(val, cur) == Ordering::Less) {
                            min_v = Some(val.clone());
                        }
                        if max_v.as_ref().is_none_or(|cur| compare_values_total(val, cur) == Ordering::Greater) {
                            max_v = Some(val.clone());
                        }
                    }
                }
            }
            ctx.explain.rows_returned = 1;
            Ok(QueryResult::MinMax { min: min_v, max: max_v })
        }
        TerminalSpec::GroupBy { field, counts_only } => {
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut fmask = FieldMask::empty();
            fmask.insert(*field);
            if *counts_only {
                let mut counts_map: FxHashMap<Vec<u8>, (CanonicalValue, u64)> = FxHashMap::default();
                while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                    for i in 0..batch.len as usize {
                        let ek = batch.keys[i];
                        ctx.recorder.record_point_entity(ek, &fmask);
                        let key_val = ctx
                            .load_row(ek)?
                            .and_then(|r| r.fields.get(field).cloned())
                            .unwrap_or(CanonicalValue::Null);
                        let gkey = canonical_group_key(&key_val);
                        if let Some((_, c)) = counts_map.get_mut(&gkey) {
                            *c += 1;
                        } else {
                            ctx.charge_heap(48 + gkey.len() + estimate_value_bytes(&key_val))?;
                            counts_map.insert(gkey, (key_val, 1));
                        }
                    }
                }
                let mut counts: Vec<(CanonicalValue, u64)> = counts_map.into_values().collect();
                counts.sort_by(|a, b| compare_values_total(&a.0, &b.0));
                ctx.explain.rows_returned = counts.len() as u64;
                Ok(QueryResult::GroupCounts(counts))
            } else {
                let mut groups_map: FxHashMap<Vec<u8>, (CanonicalValue, Vec<EntityKey>)> = FxHashMap::default();
                while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                    for i in 0..batch.len as usize {
                        let ek = batch.keys[i];
                        ctx.recorder.record_point_entity(ek, &fmask);
                        let key_val = ctx
                            .load_row(ek)?
                            .and_then(|r| r.fields.get(field).cloned())
                            .unwrap_or(CanonicalValue::Null);
                        let gkey = canonical_group_key(&key_val);
                        if let Some((_, members)) = groups_map.get_mut(&gkey) {
                            ctx.charge_heap(8)?;
                            members.push(ek);
                        } else {
                            ctx.charge_heap(64 + gkey.len() + estimate_value_bytes(&key_val))?;
                            groups_map.insert(gkey, (key_val, vec![ek]));
                        }
                    }
                }
                let mut groups: Vec<(CanonicalValue, Vec<EntityKey>)> = groups_map.into_values().collect();
                groups.sort_by(|a, b| compare_values_total(&a.0, &b.0));
                ctx.explain.rows_returned = groups.len() as u64;
                Ok(QueryResult::Groups(groups))
            }
        }
        TerminalSpec::Degrees { rel_kinds, dir } => {
            let include_trashed = source_includes_trashed(input);
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut out = Vec::new();
            while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                for i in 0..batch.len as usize {
                    let ek = batch.keys[i];
                    if !is_entity_visible(ctx, ek, include_trashed)? {
                        continue;
                    }
                    ctx.recorder.record_edge_adj(ek, rel_kinds, *dir);
                    let mut deg = 0u64;
                    if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
                        for (dst, _, _) in scan_adj_out_raw(ctx.txn, &ctx.dbs, ek, rel_kinds)? {
                            if is_entity_visible(ctx, dst, include_trashed)? {
                                deg += 1;
                            }
                        }
                    }
                    if matches!(dir, EdgeDir::In | EdgeDir::Both) {
                        for (src, _, _) in scan_adj_in_raw(ctx.txn, &ctx.dbs, ek, rel_kinds)? {
                            if is_entity_visible(ctx, src, include_trashed)? {
                                deg += 1;
                            }
                        }
                    }
                    ctx.charge_heap(16)?;
                    out.push((ek, deg));
                }
            }
            ctx.explain.rows_returned = out.len() as u64;
            Ok(QueryResult::Degrees(out))
        }
    }
}

fn collect_all_pairs<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    mut stream: VecStreamOp,
    demand: u16,
) -> Result<Vec<(EntityKey, f64)>> {
    let mut batch = KeyBatch::default();
    let mut out = Vec::new();
    while stream.next_batch(ctx, demand, &mut batch)? {
        ctx.charge_heap((batch.len as usize) * 16)?;
        for i in 0..batch.len as usize {
            out.push((batch.keys[i], batch.scores[i]));
        }
    }
    Ok(out)
}

/// Evaluates any stream-producing `LogicalNode` into a `VecStreamOp` with optional `limit_hint`
/// enabling Top-K, BMW, HNSW, reverse index order, and `demand=1` early stop pushdowns!
fn eval_stream<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    node: &LogicalNode,
    limit_hint: Option<u64>,
) -> Result<VecStreamOp> {
    ctx.check_cancel()?;
    match node {
        LogicalNode::Source(src) => eval_source(ctx, src, limit_hint),
        LogicalNode::Filter { input, expr } => eval_filter(ctx, input, expr, limit_hint),
        LogicalNode::Score { input, spec } => eval_score(ctx, input, spec, limit_hint),
        LogicalNode::Traverse { input, spec } => eval_traverse(ctx, input, spec),
        LogicalNode::Distinct { input } => {
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut seen = FxHashSet::default();
            let mut out = Vec::new();
            while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                for i in 0..batch.len as usize {
                    let k = batch.keys[i];
                    if seen.insert(k) {
                        ctx.charge_heap(24)?;
                        out.push((k, batch.scores[i]));
                        if let Some(lim) = limit_hint
                            && out.len() as u64 >= lim
                        {
                            return Ok(VecStreamOp::new(out));
                        }
                    }
                }
            }
            Ok(VecStreamOp::new(out))
        }
        LogicalNode::OrderSlice { input, order, after, limit } => {
            eval_order_slice(ctx, input, order, after.as_ref(), *limit)
        }
        LogicalNode::GraphAnalyze { .. } | LogicalNode::Terminal { .. } => {
            Err(ElmdbError::new(ErrorCode::InvalidArgument, "unexpected terminal/graph node inside stream"))
        }
    }
}

fn eval_source<R: KvRead>(ctx: &mut ExecCtx<'_, R>, src: &SourceSpec, limit_hint: Option<u64>) -> Result<VecStreamOp> {
    let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    if cap == 0 {
        return Ok(VecStreamOp::new(Vec::new()));
    }
    let mut out = Vec::new();
    match src {
        SourceSpec::Type { type_id, include_trashed } => {
            ctx.recorder.record_type_scan(*type_id);
            let mut after: Option<EntityKey> = None;
            loop {
                ctx.check_cancel()?;
                let chunk_cap = if *include_trashed { (cap - out.len()).min(BATCH_SIZE) } else { BATCH_SIZE };
                let keys = scan_type_keys_in_range(ctx.txn, &ctx.dbs, *type_id, after, None, chunk_cap)?;
                if keys.is_empty() {
                    break;
                }
                let full_chunk = keys.len() == chunk_cap;
                after = keys.last().copied();
                for ek in keys {
                    ctx.check_cancel()?;
                    if !*include_trashed {
                        let Some(row) = ctx.load_row(ek)? else { continue };
                        if row.header.is_trashed() {
                            continue;
                        }
                    }
                    ctx.charge_heap(16)?;
                    out.push((ek, 0.0));
                    if out.len() >= cap {
                        return Ok(VecStreamOp::new(out));
                    }
                }
                if !full_chunk {
                    break;
                }
            }
        }
        SourceSpec::Types { type_ids, include_trashed } => {
            // Deduplicate `type_ids` first and never early-truncate before sorting across multiple types!
            let mut uniq_types: Vec<TypeId> = type_ids.clone();
            uniq_types.sort_unstable();
            uniq_types.dedup();
            for tid in uniq_types {
                ctx.recorder.record_type_scan(tid);
                let mut after: Option<EntityKey> = None;
                loop {
                    ctx.check_cancel()?;
                    let keys = scan_type_keys_in_range(ctx.txn, &ctx.dbs, tid, after, None, BATCH_SIZE)?;
                    if keys.is_empty() {
                        break;
                    }
                    let full_chunk = keys.len() == BATCH_SIZE;
                    after = keys.last().copied();
                    for ek in keys {
                        ctx.check_cancel()?;
                        if !*include_trashed {
                            let Some(row) = ctx.load_row(ek)? else { continue };
                            if row.header.is_trashed() {
                                continue;
                            }
                        }
                        ctx.charge_heap(16)?;
                        out.push((ek, 0.0));
                    }
                    if !full_chunk {
                        break;
                    }
                }
            }
            out.sort_by_key(|(k, _)| *k);
            out.dedup_by_key(|(k, _)| *k);
            out.truncate(cap);
        }
        SourceSpec::AllEntities { include_trashed } => {
            ctx.recorder.record_all_entities();
            let mut cur = ctx.txn.cursor(ctx.dbs.entities)?;
            let mut step = cur.first()?;
            while let Some((k, _)) = step {
                ctx.check_cancel()?;
                if k.len() == 8 {
                    let ek = EntityKey::from_ne_bytes(k.try_into().expect("8"));
                    if !*include_trashed {
                        if let Some(row) = ctx.load_row(ek)?
                            && !row.header.is_trashed()
                        {
                            ctx.charge_heap(16)?;
                            out.push((ek, 0.0));
                        }
                    } else {
                        ctx.charge_heap(16)?;
                        out.push((ek, 0.0));
                    }
                    if out.len() >= cap {
                        break;
                    }
                }
                step = cur.next()?;
            }
        }
        SourceSpec::LiteralKeys(keys) => {
            let empty_mask = FieldMask::empty();
            for &ek in keys {
                ctx.check_cancel()?;
                // Record existence dependency even when absent so creating/destroying `ek` invalidates watches!
                ctx.recorder.record_point_entity(ek, &empty_mask);
                if ctx.txn.get(ctx.dbs.entities, &ek.to_ne_bytes())?.is_some() {
                    ctx.charge_heap(16)?;
                    out.push((ek, 0.0));
                    if out.len() >= cap {
                        break;
                    }
                }
            }
        }
    }
    Ok(VecStreamOp::new(out))
}

fn eval_filter<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    input: &LogicalNode,
    expr: &FilterExpr,
    limit_hint: Option<u64>,
) -> Result<VecStreamOp> {
    // Attempt index/trigram/HasLink pushdown when sitting directly above a live single-type Source!
    if let LogicalNode::Source(SourceSpec::Type { type_id, include_trashed: false }) = input {
        let mut conjuncts = Vec::new();
        flatten_and(expr, &mut conjuncts);

        let mut index_lists: Vec<Vec<EntityKey>> = Vec::new();
        let mut residual: Vec<FilterExpr> = Vec::new();

        for c in conjuncts {
            ctx.check_cancel()?;
            if let Some((keys, needs_residual, label)) = try_pushdown_conjunct(ctx, *type_id, &c)? {
                ctx.charge_heap(keys.len().saturating_mul(8))?;
                ctx.explain.pushed_filters.push(label);
                index_lists.push(keys);
                if needs_residual {
                    residual.push(c);
                }
            } else {
                residual.push(c);
            }
        }

        if !index_lists.is_empty() {
            ctx.check_cancel()?;
            let candidates = leapfrog_intersect(index_lists);
            ctx.explain.residual_filters += residual.len();
            if residual.is_empty() {
                let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
                let mut pairs = Vec::with_capacity(candidates.len().min(cap));
                for k in candidates.into_iter().take(cap) {
                    ctx.check_cancel()?;
                    ctx.charge_heap(16)?;
                    pairs.push((k, 0.0));
                }
                return Ok(VecStreamOp::new(pairs));
            }
            let res_expr = if residual.len() == 1 { residual.pop().unwrap() } else { FilterExpr::And(residual) };
            let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
            let mut out = Vec::new();
            for ek in candidates {
                ctx.check_cancel()?;
                if eval_predicate(ctx, ek, &res_expr, false)? {
                    ctx.charge_heap(16)?;
                    out.push((ek, 0.0));
                    if out.len() >= cap {
                        break;
                    }
                }
            }
            return Ok(VecStreamOp::new(out));
        }
    }

    // Fallback: pull from input stream and apply predicate.
    let include_trashed = source_includes_trashed(input);
    let mut input_stream = eval_stream(ctx, input, None)?;
    ctx.explain.residual_filters += 1;
    let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    let mut batch = KeyBatch::default();
    let mut out = Vec::new();
    while input_stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
        for i in 0..batch.len as usize {
            let ek = batch.keys[i];
            if eval_predicate(ctx, ek, expr, include_trashed)? {
                ctx.charge_heap(16)?;
                out.push((ek, batch.scores[i]));
                if out.len() >= cap {
                    return Ok(VecStreamOp::new(out));
                }
            }
        }
    }
    Ok(VecStreamOp::new(out))
}

/// Attempts to push one `And` conjunct into a Ready Value index, Trigram index, or smaller-side `HasLink` adjacency!
/// Returns `Some((candidate_keys, needs_residual_verification, explain_label))`.
fn try_pushdown_conjunct<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    type_id: TypeId,
    conjunct: &FilterExpr,
) -> Result<Option<(Vec<EntityKey>, bool, String)>> {
    match conjunct {
        FilterExpr::Eq(field, val) => {
            if matches!(val, CanonicalValue::Null) {
                return Ok(None);
            }
            let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            let is_single = matches!(&idx.kind, IndexKind::Value { fields, .. } if fields.len() == 1);
            match value::encode_compound_values(&[val]) {
                ValueKeyClass::Indexed(_) => {
                    let mut keys = if is_single {
                        ctx.recorder.record_indexed_eq(idx.id, hash_canonical_value(val));
                        value::lookup_eq(ctx.txn, db, &[val])?
                    } else {
                        // Compound index: prefix scan covers TAG_MAIN + aux partitions (TAG_WITHOUT/TAG_UNORDERED/TAG_LONG)
                        // because rows with missing/unordered trailing fields live in TAG_WITHOUT/TAG_UNORDERED!
                        ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x05]);
                        value::scan_range(ctx.txn, db, Some((&[val], true)), Some((&[val], true)), false, None)?
                    };
                    ctx.recorder.record_subdb_range(idx.slot, &[0x04], &[0x05]);
                    let long_keys = value::scan_long_keys(ctx.txn, db)?;
                    let mut needs_residual = !long_keys.is_empty();
                    keys.extend(long_keys);
                    if !is_single {
                        let without_keys = value::scan_without_keys(ctx.txn, db)?;
                        let unordered_keys = value::scan_unordered_keys(ctx.txn, db)?;
                        if !without_keys.is_empty() || !unordered_keys.is_empty() {
                            needs_residual = true;
                            keys.extend(without_keys);
                            keys.extend(unordered_keys);
                        }
                    }
                    Ok(Some((keys, needs_residual, format!("ValueEq({})", idx.name))))
                }
                ValueKeyClass::Long => {
                    // Long string > 448 B lives in `TAG_LONG` (and for compound indexes, possibly TAG_WITHOUT/TAG_UNORDERED).
                    ctx.recorder.record_subdb_range(idx.slot, &[0x02], &[0x05]);
                    let mut keys = value::scan_long_keys(ctx.txn, db)?;
                    if !is_single {
                        keys.extend(value::scan_without_keys(ctx.txn, db)?);
                        keys.extend(value::scan_unordered_keys(ctx.txn, db)?);
                    }
                    Ok(Some((keys, true, format!("ValueLongEq({})", idx.name))))
                }
                _ => Ok(None),
            }
        }
        FilterExpr::In(field, vals) => {
            if vals.is_empty() {
                return Ok(Some((Vec::new(), false, "EmptyIn".into())));
            }
            if vals.iter().any(|v| {
                matches!(
                    v,
                    CanonicalValue::Null
                        | CanonicalValue::Object(_)
                        | CanonicalValue::Array(_)
                        | CanonicalValue::Bytes(_)
                )
            }) {
                return Ok(None);
            }
            let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            let is_single = matches!(&idx.kind, IndexKind::Value { fields, .. } if fields.len() == 1);
            let mut keys = Vec::new();
            let mut has_long = false;
            for v in vals {
                match value::encode_compound_values(&[v]) {
                    ValueKeyClass::Indexed(_) => {
                        if is_single {
                            ctx.recorder.record_indexed_eq(idx.id, hash_canonical_value(v));
                            keys.extend(value::lookup_eq(ctx.txn, db, &[v])?);
                        } else {
                            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x05]);
                            keys.extend(value::scan_range(
                                ctx.txn,
                                db,
                                Some((&[v], true)),
                                Some((&[v], true)),
                                false,
                                None,
                            )?);
                        }
                    }
                    ValueKeyClass::Long => has_long = true,
                    _ => return Ok(None),
                }
            }
            ctx.recorder.record_subdb_range(idx.slot, &[0x04], &[0x05]);
            let long_keys = value::scan_long_keys(ctx.txn, db)?;
            let mut needs_residual = has_long || !long_keys.is_empty();
            keys.extend(long_keys);
            if !is_single {
                ctx.recorder.record_subdb_range(idx.slot, &[0x02], &[0x05]);
                let without_keys = value::scan_without_keys(ctx.txn, db)?;
                let unordered_keys = value::scan_unordered_keys(ctx.txn, db)?;
                if !without_keys.is_empty() || !unordered_keys.is_empty() {
                    needs_residual = true;
                    keys.extend(without_keys);
                    keys.extend(unordered_keys);
                }
            }
            Ok(Some((keys, needs_residual, format!("ValueIn({})", idx.name))))
        }
        FilterExpr::Range { field, low, high } => {
            // Fully open range matches TAG_UNORDERED too; fall back to primary scan for soundness.
            if low.is_none() && high.is_none() {
                return Ok(None);
            }
            // Cross-type bounds (e.g. Int(5) .. String("z") on an Any field) can never satisfy both bounds!
            if let (Some((lv, _)), Some((hv, _))) = (low, high)
                && compare_values(lv, hv).is_none()
            {
                return Ok(Some((Vec::new(), false, "EmptyRange".into())));
            }
            let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            let is_single = matches!(&idx.kind, IndexKind::Value { fields, .. } if fields.len() == 1);
            let l_arr;
            let h_arr;
            let lo = match low {
                Some((v, inc)) => {
                    if !matches!(value::encode_compound_values(&[v]), ValueKeyClass::Indexed(_)) {
                        return Ok(None);
                    }
                    l_arr = [v];
                    Some((l_arr.as_slice(), *inc))
                }
                None => None,
            };
            let hi = match high {
                Some((v, inc)) => {
                    if !matches!(value::encode_compound_values(&[v]), ValueKeyClass::Indexed(_)) {
                        return Ok(None);
                    }
                    h_arr = [v];
                    Some((h_arr.as_slice(), *inc))
                }
                None => None,
            };
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
            ctx.recorder.record_subdb_range(idx.slot, &[0x04], &[0x05]);
            let mut keys = value::scan_range(ctx.txn, db, lo, hi, false, None)?;
            let long_keys = value::scan_long_keys(ctx.txn, db)?;
            let mut needs_residual = !long_keys.is_empty();
            keys.extend(long_keys);
            if !is_single {
                ctx.recorder.record_subdb_range(idx.slot, &[0x02], &[0x04]);
                let without_keys = value::scan_without_keys(ctx.txn, db)?;
                let unordered_keys = value::scan_unordered_keys(ctx.txn, db)?;
                if !without_keys.is_empty() || !unordered_keys.is_empty() {
                    needs_residual = true;
                    keys.extend(without_keys);
                    keys.extend(unordered_keys);
                }
            }
            Ok(Some((keys, needs_residual, format!("ValueRange({})", idx.name))))
        }
        FilterExpr::StartsWith(field, prefix) => {
            let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            let is_single = matches!(&idx.kind, IndexKind::Value { fields, .. } if fields.len() == 1);
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
            ctx.recorder.record_subdb_range(idx.slot, &[0x04], &[0x05]);
            let mut keys = value::scan_prefix_string(ctx.txn, db, prefix, None)?;
            let long_keys = value::scan_long_keys(ctx.txn, db)?;
            let mut needs_residual = !long_keys.is_empty();
            keys.extend(long_keys);
            if !is_single {
                ctx.recorder.record_subdb_range(idx.slot, &[0x02], &[0x04]);
                let without_keys = value::scan_without_keys(ctx.txn, db)?;
                let unordered_keys = value::scan_unordered_keys(ctx.txn, db)?;
                if !without_keys.is_empty() || !unordered_keys.is_empty() {
                    needs_residual = true;
                    keys.extend(without_keys);
                    keys.extend(unordered_keys);
                }
            }
            Ok(Some((keys, needs_residual, format!("ValuePrefix({})", idx.name))))
        }
        FilterExpr::Contains(field, needle) => {
            let trigs = trigram::extract_trigrams(needle);
            if trigs.is_empty() {
                return Ok(None);
            }
            let Some(idx) = find_ready_trigram_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            // Cover TAG_TRIGRAM (0x01), TAG_TRIG_DF (0x02), TAG_DOC_TRIGS (0x03), TAG_OVERFLOW (0x04).
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x05]);
            let keys = trigram::intersect_trigrams(ctx.txn, db, &trigs)?;
            Ok(Some((keys, true, format!("TrigramContains({})", idx.name))))
        }
        FilterExpr::Regex(field, pattern) => {
            let trigs = extract_regex_trigrams(pattern);
            if trigs.is_empty() {
                return Ok(None);
            }
            let Some(idx) = find_ready_trigram_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x05]);
            let keys = trigram::intersect_trigrams(ctx.txn, db, &trigs)?;
            Ok(Some((keys, true, format!("TrigramRegex({})", idx.name))))
        }
        FilterExpr::HasLink { rel_kinds, dir, target: Some(target_key) } => {
            // Drive-from-smaller-side for `HasLink { target: X }`: verify target is live & record dependency!
            if !is_entity_visible(ctx, *target_key, false)? {
                return Ok(Some((Vec::new(), false, "HasLinkDriveFromTarget".into())));
            }
            let rev_dir = match dir {
                EdgeDir::Out => EdgeDir::In,
                EdgeDir::In => EdgeDir::Out,
                EdgeDir::Both => EdgeDir::Both,
            };
            ctx.recorder.record_edge_adj(*target_key, rel_kinds, rev_dir);
            let empty_mask = FieldMask::empty();
            let mut linked = Vec::new();
            if matches!(rev_dir, EdgeDir::In | EdgeDir::Both) {
                for (src, _, _) in scan_adj_in_raw(ctx.txn, &ctx.dbs, *target_key, rel_kinds)? {
                    // Record liveness/trash dependency for EVERY adjacent neighbor inspected!
                    ctx.recorder.record_point_entity(src, &empty_mask);
                    if let Some(row) = ctx.load_row(src)?
                        && row.header.type_id == type_id
                        && !row.header.is_trashed()
                    {
                        linked.push(src);
                    }
                }
            }
            if matches!(rev_dir, EdgeDir::Out | EdgeDir::Both) {
                for (dst, _, _) in scan_adj_out_raw(ctx.txn, &ctx.dbs, *target_key, rel_kinds)? {
                    ctx.recorder.record_point_entity(dst, &empty_mask);
                    if let Some(row) = ctx.load_row(dst)?
                        && row.header.type_id == type_id
                        && !row.header.is_trashed()
                    {
                        linked.push(dst);
                    }
                }
            }
            Ok(Some((linked, false, "HasLinkDriveFromTarget".into())))
        }
        _ => Ok(None),
    }
}

/// Shared subject/neighbor visibility check used uniformly across edge and graph operators.
fn is_entity_visible<R: KvRead>(ctx: &mut ExecCtx<'_, R>, key: EntityKey, include_trashed: bool) -> Result<bool> {
    ctx.recorder.record_point_entity(key, &FieldMask::empty());
    let Some(row) = ctx.load_row(key)? else {
        return Ok(false);
    };
    Ok(include_trashed || !row.header.is_trashed())
}

fn eval_predicate<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    key: EntityKey,
    expr: &FilterExpr,
    include_trashed: bool,
) -> Result<bool> {
    match expr {
        FilterExpr::Eq(field, target) => {
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
            match row.fields.get(field) {
                Some(v) => Ok(values_equal(v, target)),
                None => Ok(matches!(target, CanonicalValue::Null)),
            }
        }
        FilterExpr::Ne(field, target) => {
            Ok(!eval_predicate(ctx, key, &FilterExpr::Eq(*field, target.clone()), include_trashed)?)
        }
        FilterExpr::In(field, targets) => {
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
            let val = row.fields.get(field).unwrap_or(&CanonicalValue::Null);
            Ok(targets.iter().any(|t| values_equal(val, t)))
        }
        FilterExpr::Range { field, low, high } => {
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
            let Some(val) = row.fields.get(field) else { return Ok(false) };
            if matches!(val, CanonicalValue::Null) {
                return Ok(false);
            }
            if let Some((lb, inc)) = low {
                match compare_values(val, lb) {
                    Some(Ordering::Less) => return Ok(false),
                    Some(Ordering::Equal) if !inc => return Ok(false),
                    None => return Ok(false),
                    _ => {}
                }
            }
            if let Some((hb, inc)) = high {
                match compare_values(val, hb) {
                    Some(Ordering::Greater) => return Ok(false),
                    Some(Ordering::Equal) if !inc => return Ok(false),
                    None => return Ok(false),
                    _ => {}
                }
            }
            Ok(true)
        }
        FilterExpr::StartsWith(field, prefix) => {
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
            Ok(matches!(row.fields.get(field), Some(CanonicalValue::String(s)) if s.starts_with(prefix.as_str())))
        }
        FilterExpr::Contains(field, needle) => {
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
            let low_needle = trigram::fold_case(needle);
            match row.fields.get(field) {
                Some(CanonicalValue::String(s)) => Ok(trigram::fold_case(s).contains(&low_needle)),
                Some(CanonicalValue::Array(items)) => Ok(items
                    .iter()
                    .any(|it| matches!(it, CanonicalValue::String(s) if trigram::fold_case(s).contains(&low_needle)))),
                _ => Ok(false),
            }
        }
        FilterExpr::Regex(field, pattern) => {
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
            let re = ctx.regex_cache.get(pattern).ok_or_else(|| {
                ElmdbError::new(ErrorCode::Internal, format!("missing precompiled regex '{pattern}'"))
            })?;
            match row.fields.get(field) {
                Some(CanonicalValue::String(s)) => Ok(re.is_match(s)),
                Some(CanonicalValue::Array(items)) => {
                    Ok(items.iter().any(|it| matches!(it, CanonicalValue::String(s) if re.is_match(s))))
                }
                _ => Ok(false),
            }
        }
        FilterExpr::IsNull(field) => {
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
            Ok(matches!(row.fields.get(field), None | Some(CanonicalValue::Null)))
        }
        FilterExpr::IsPresent(field) => Ok(!eval_predicate(ctx, key, &FilterExpr::IsNull(*field), include_trashed)?),
        FilterExpr::HasLink { rel_kinds, dir, target } => {
            if !is_entity_visible(ctx, key, include_trashed)? {
                return Ok(false);
            }
            if let Some(t) = target
                && !is_entity_visible(ctx, *t, include_trashed)?
            {
                return Ok(false);
            }
            ctx.recorder.record_edge_adj(key, rel_kinds, *dir);
            if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
                let edges = scan_adj_out_raw(ctx.txn, &ctx.dbs, key, rel_kinds)?;
                for (dst, _, _) in edges {
                    if target.is_none_or(|t| t == dst) && is_entity_visible(ctx, dst, include_trashed)? {
                        return Ok(true);
                    }
                }
            }
            if matches!(dir, EdgeDir::In | EdgeDir::Both) {
                let edges = scan_adj_in_raw(ctx.txn, &ctx.dbs, key, rel_kinds)?;
                for (src, _, _) in edges {
                    if target.is_none_or(|t| t == src) && is_entity_visible(ctx, src, include_trashed)? {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }
        FilterExpr::And(parts) => {
            for p in parts {
                if !eval_predicate(ctx, key, p, include_trashed)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        FilterExpr::Or(parts) => {
            for p in parts {
                if eval_predicate(ctx, key, p, include_trashed)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        FilterExpr::Not(inner) => Ok(!eval_predicate(ctx, key, inner, include_trashed)?),
    }
}

fn score_index_id(spec: &ScoreSpec) -> Option<IndexId> {
    match spec {
        ScoreSpec::Bm25f { index_id, .. }
        | ScoreSpec::VectorNear { index_id, .. }
        | ScoreSpec::VectorLike { index_id, .. }
        | ScoreSpec::RankIndex { index_id } => Some(*index_id),
        ScoreSpec::FuzzyLabel { .. } | ScoreSpec::RecencyDecay { .. } | ScoreSpec::Fuse { .. } => None,
    }
}

fn eval_score<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    input: &LogicalNode,
    spec: &ScoreSpec,
    limit_hint: Option<u64>,
) -> Result<VecStreamOp> {
    // Allow direct index-driven BMW/HNSW/Rank ONLY when `input` is an unfiltered live single-type Source
    // AND the target index belongs to that exact `type_id` (preventing cross-type leaks!).
    let cand_set: Option<FxHashSet<EntityKey>> = match input {
        LogicalNode::Source(SourceSpec::Type { type_id, include_trashed: false }) if score_index_id(spec).is_some() => {
            let idx_id = score_index_id(spec).expect("checked");
            let def = ctx
                .schema
                .index_by_id(idx_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", idx_id.0)))?;
            if def.type_id != *type_id {
                return Ok(VecStreamOp::new(Vec::new()));
            }
            None
        }
        _ => {
            let stream = eval_stream(ctx, input, None)?;
            let pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;
            ctx.charge_heap(pairs.len().saturating_mul(16))?;
            Some(pairs.into_iter().map(|(k, _)| k).collect())
        }
    };

    let scored = eval_score_signal(ctx, input, spec, cand_set.as_ref(), limit_hint)?;
    Ok(VecStreamOp::new(scored))
}

fn eval_score_signal<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    input: &LogicalNode,
    spec: &ScoreSpec,
    candidates: Option<&FxHashSet<EntityKey>>,
    limit_hint: Option<u64>,
) -> Result<Vec<(EntityKey, f64)>> {
    ctx.check_cancel()?;
    match spec {
        ScoreSpec::Bm25f { index_id, query, require_all, prefix, phrase, top_k } => {
            let def = ctx
                .schema
                .index_by_id(*index_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", index_id.0)))?;
            if !matches!(&def.kind, IndexKind::Text { .. }) {
                return Err(ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    format!("index '{}' is not a Text index", def.name),
                ));
            }
            // Record dependency BEFORE checking empty/uninitialized sub-DB so watches fire when first docs arrive!
            ctx.recorder.record_subdb_range(def.slot, &[0x00], &[0x06]);
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "text index is not ready"));
            }
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            let k = top_k.or_else(|| limit_hint.map(|l| l.min(usize::MAX as u64) as usize)).unwrap_or(usize::MAX);
            if k == 0 {
                return Ok(Vec::new());
            }
            let normalized_phrase: Option<Vec<(String, u32)>> =
                phrase.as_ref().map(|terms| text::analyze_phrase(terms, None));

            if let Some(phrase_terms) = normalized_phrase {
                let query_is_empty = text::analyze_text(query, None).is_empty();
                if query_is_empty && phrase_terms.is_empty() {
                    return Ok(Vec::new());
                }
                let phrase_query_owned: Option<String> = if query_is_empty {
                    Some(phrase_terms.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(" "))
                } else {
                    None
                };
                let effective_query = phrase_query_owned.as_deref().unwrap_or(query);
                let effective_require_all = if query_is_empty { true } else { *require_all };
                let effective_prefix = if query_is_empty { false } else { *prefix };

                // Exponential widening ensures zero phrase false negatives even when > 128 higher-scoring
                // non-phrase distractors exist, while staying fast for normal queries!
                let mut fetch_k = k.saturating_mul(4).clamp(128, 1_048_576);
                loop {
                    ctx.check_cancel()?;
                    let (hits, _) = text::search_bmw_with_prefix(
                        ctx.txn,
                        db,
                        effective_query,
                        None,
                        fetch_k,
                        effective_require_all,
                        effective_prefix,
                        candidates,
                    )?;
                    let exhausted = hits.len() < fetch_k;
                    let mut verified = Vec::new();
                    for h in hits {
                        ctx.check_cancel()?;
                        if text::matches_phrase_positions(ctx.txn, db, h.key, &phrase_terms)? {
                            ctx.charge_heap(16)?;
                            verified.push((h.key, h.score));
                            if verified.len() >= k {
                                return Ok(verified);
                            }
                        }
                    }
                    if exhausted || fetch_k == usize::MAX {
                        return Ok(verified);
                    }
                    fetch_k = fetch_k.saturating_mul(4);
                }
            } else {
                let (hits, _) =
                    text::search_bmw_with_prefix(ctx.txn, db, query, None, k, *require_all, *prefix, candidates)?;
                ctx.charge_heap(hits.len().saturating_mul(16))?;
                Ok(hits.into_iter().map(|h| (h.key, h.score)).collect())
            }
        }
        ScoreSpec::VectorNear { index_id, vector, top_k } => {
            let def = ctx
                .schema
                .index_by_id(*index_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", index_id.0)))?;
            if !matches!(&def.kind, IndexKind::Vector { .. }) {
                return Err(ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    format!("index '{}' is not a Vector index", def.name),
                ));
            }
            ctx.recorder.record_subdb_range(def.slot, &[0x00], &[0x05]);
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "vector index is not ready"));
            }
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            let k = (*top_k).min(limit_hint.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize));
            let hits = search_near(
                ctx.txn,
                &ctx.dbs,
                def,
                db,
                vector,
                SearchNearOpts { top_k: k, candidates, best_chunk_per_entity: true },
            )?;
            ctx.charge_heap(hits.len().saturating_mul(16))?;
            Ok(hits.into_iter().map(|h| (h.key, h.score as f64)).collect())
        }
        ScoreSpec::VectorLike { index_id, seed, top_k } => {
            let def = ctx
                .schema
                .index_by_id(*index_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", index_id.0)))?;
            if !matches!(&def.kind, IndexKind::Vector { .. }) {
                return Err(ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    format!("index '{}' is not a Vector index", def.name),
                ));
            }
            ctx.recorder.record_subdb_range(def.slot, &[0x00], &[0x05]);
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "vector index is not ready"));
            }
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            let hits = search_like(ctx.txn, &ctx.dbs, def, db, *seed, *top_k, candidates)?;
            ctx.charge_heap(hits.len().saturating_mul(16))?;
            Ok(hits.into_iter().map(|h| (h.key, h.score as f64)).collect())
        }
        ScoreSpec::RankIndex { index_id } => {
            let def = ctx
                .schema
                .index_by_id(*index_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", index_id.0)))?;
            if !matches!(&def.kind, IndexKind::Rank { .. }) {
                return Err(ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    format!("index '{}' is not a Rank index", def.name),
                ));
            }
            ctx.recorder.record_subdb_range(def.slot, &[0x00], &[0x05]);
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "rank index is not ready"));
            }
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            match candidates {
                None => {
                    let lim = limit_hint.map(|l| l.min(usize::MAX as u64) as usize);
                    let out = rank::scan_ranked(ctx.txn, db, false, lim)?;
                    ctx.charge_heap(out.len().saturating_mul(16))?;
                    Ok(out)
                }
                Some(allowed) => {
                    let mut out = Vec::with_capacity(allowed.len());
                    for &ek in allowed {
                        ctx.check_cancel()?;
                        if let Some(score) = rank::rank_of(ctx.txn, db, ek)? {
                            ctx.charge_heap(16)?;
                            out.push((ek, score));
                        }
                    }
                    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                    if let Some(l) = limit_hint {
                        out.truncate(l.min(usize::MAX as u64) as usize);
                    }
                    Ok(out)
                }
            }
        }
        ScoreSpec::FuzzyLabel { field, query, max_distance } => {
            let keys: Vec<EntityKey> = match candidates {
                Some(set) => set.iter().copied().collect(),
                None => {
                    let s = eval_stream(ctx, input, None)?;
                    collect_all_pairs(ctx, s, BATCH_SIZE as u16)?.into_iter().map(|(k, _)| k).collect()
                }
            };
            let dist_cap = (*max_distance).min(2);
            let builder = LevenshteinAutomatonBuilder::new(dist_cap, true);
            let q_low = query.to_lowercase();
            let dfa = builder.build_dfa(&q_low);
            let mut fmask = FieldMask::empty();
            fmask.insert(*field);
            let mut out = Vec::new();
            for ek in keys {
                ctx.check_cancel()?;
                ctx.recorder.record_point_entity(ek, &fmask);
                if let Some(row) = ctx.load_row(ek)?
                    && let Some(CanonicalValue::String(s)) = row.fields.get(field)
                {
                    let cand_low = s.to_lowercase();
                    match dfa.eval(&cand_low) {
                        Distance::Exact(d) => {
                            ctx.charge_heap(16)?;
                            let score = 1.0 / (1.0 + (d as f64));
                            out.push((ek, score));
                        }
                        Distance::AtLeast(_) => {}
                    }
                }
            }
            out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(out)
        }
        ScoreSpec::RecencyDecay { field, now_ms, half_life_ms } => {
            if !half_life_ms.is_finite() || *half_life_ms <= 0.0 {
                return Err(ElmdbError::new(ErrorCode::InvalidArgument, "half_life_ms must be > 0"));
            }
            let keys: Vec<EntityKey> = match candidates {
                Some(set) => set.iter().copied().collect(),
                None => {
                    let s = eval_stream(ctx, input, None)?;
                    collect_all_pairs(ctx, s, BATCH_SIZE as u16)?.into_iter().map(|(k, _)| k).collect()
                }
            };
            let mut fmask = FieldMask::empty();
            fmask.insert(*field);
            let mut out = Vec::with_capacity(keys.len());
            for ek in keys {
                ctx.check_cancel()?;
                ctx.recorder.record_point_entity(ek, &fmask);
                if let Some(row) = ctx.load_row(ek)?
                    && let Some(val) = row.fields.get(field)
                {
                    let ts_ms = match val {
                        CanonicalValue::Timestamp { ms, .. } => Some(*ms),
                        CanonicalValue::Int(ms) => Some(*ms),
                        _ => None,
                    };
                    if let Some(ms) = ts_ms {
                        ctx.charge_heap(16)?;
                        let age = now_ms.saturating_sub(ms).max(0) as f64;
                        let score = 0.5f64.powf(age / *half_life_ms);
                        out.push((ek, score));
                    }
                }
            }
            out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(out)
        }
        ScoreSpec::Fuse { inputs, method } => {
            if inputs.is_empty() {
                return Ok(Vec::new());
            }
            for (_, weight) in inputs {
                if !weight.is_finite() || *weight < 0.0 {
                    return Err(ElmdbError::new(ErrorCode::InvalidArgument, "Fuse weight must be finite and >= 0"));
                }
            }
            let mut fused: FxHashMap<EntityKey, f64> = FxHashMap::default();
            match *method {
                FuseMethod::Rrf { k } => {
                    if !k.is_finite() || k <= 0.0 {
                        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "RRF k must be > 0"));
                    }
                    for (sub_spec, weight) in inputs {
                        ctx.check_cancel()?;
                        if *weight == 0.0 {
                            continue;
                        }
                        let mut sub_hits = eval_score_signal(ctx, input, sub_spec, candidates, None)?;
                        sub_hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                        for (rank_0, (ek, _)) in sub_hits.into_iter().enumerate() {
                            if rank_0 & 255 == 0 {
                                ctx.check_cancel()?;
                            }
                            let contrib = *weight / (k + (rank_0 as f64) + 1.0);
                            let prev_len = fused.len();
                            *fused.entry(ek).or_insert(0.0) += contrib;
                            if fused.len() > prev_len {
                                ctx.charge_heap(24)?;
                            }
                        }
                    }
                }
                FuseMethod::LinearNormalized => {
                    for (sub_spec, weight) in inputs {
                        ctx.check_cancel()?;
                        if *weight == 0.0 {
                            continue;
                        }
                        let sub_hits = eval_score_signal(ctx, input, sub_spec, candidates, None)?;
                        if sub_hits.is_empty() {
                            continue;
                        }
                        let mut min_s = f64::INFINITY;
                        let mut max_s = f64::NEG_INFINITY;
                        for &(_, s) in &sub_hits {
                            if s.is_finite() {
                                min_s = min_s.min(s);
                                max_s = max_s.max(s);
                            }
                        }
                        if !min_s.is_finite() || !max_s.is_finite() {
                            continue;
                        }
                        let span = max_s - min_s;
                        let overflow_span = !span.is_finite();
                        let half_min = min_s * 0.5;
                        let half_span = (max_s * 0.5) - half_min;
                        for (idx, (ek, s)) in sub_hits.into_iter().enumerate() {
                            if idx & 255 == 0 {
                                ctx.check_cancel()?;
                            }
                            if !s.is_finite() {
                                continue;
                            }
                            // Guard `max == min` (never divides by zero!) AND `max - min == Inf` overflow!
                            let norm = if min_s == max_s {
                                if s > 0.0 { 1.0 } else { 0.0 }
                            } else if overflow_span {
                                (((s * 0.5) - half_min) / half_span).clamp(0.0, 1.0)
                            } else {
                                ((s - min_s) / span).clamp(0.0, 1.0)
                            };
                            let prev_len = fused.len();
                            *fused.entry(ek).or_insert(0.0) += *weight * norm;
                            if fused.len() > prev_len {
                                ctx.charge_heap(24)?;
                            }
                        }
                    }
                }
            }
            let mut out: Vec<(EntityKey, f64)> = fused.into_iter().collect();
            out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            if let Some(lim) = limit_hint {
                out.truncate(lim.min(usize::MAX as u64) as usize);
            }
            Ok(out)
        }
    }
}

fn eval_traverse<R: KvRead>(ctx: &mut ExecCtx<'_, R>, input: &LogicalNode, spec: &TraverseSpec) -> Result<VecStreamOp> {
    let include_trashed = source_includes_trashed(input);
    let stream = eval_stream(ctx, input, None)?;
    let seeds: Vec<EntityKey> =
        collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?.into_iter().map(|(k, _)| k).collect();

    match spec {
        TraverseSpec::Hops { rel_kinds, dir, min_hops, max_hops } => {
            let mut visited: FxHashMap<EntityKey, u32> = FxHashMap::default();
            let mut hidden: FxHashSet<EntityKey> = FxHashSet::default();
            let mut queue: VecDeque<(EntityKey, u32)> = VecDeque::new();
            let mut results = Vec::new();
            for s in seeds {
                ctx.check_cancel()?;
                if visited.contains_key(&s) || hidden.contains(&s) {
                    continue;
                }
                if !is_entity_visible(ctx, s, include_trashed)? {
                    if visited.len().saturating_add(hidden.len()) >= ctx.budgets.max_traversal_visited {
                        return Err(ElmdbError::new(
                            ErrorCode::ResourceLimit,
                            "traversal exceeded max_traversal_visited budget",
                        ));
                    }
                    ctx.charge_heap(16)?;
                    hidden.insert(s);
                    continue;
                }
                if visited.len().saturating_add(hidden.len()) >= ctx.budgets.max_traversal_visited {
                    return Err(ElmdbError::new(
                        ErrorCode::ResourceLimit,
                        "traversal exceeded max_traversal_visited budget",
                    ));
                }
                visited.insert(s, 0);
                ctx.charge_heap(32)?;
                queue.push_back((s, 0));
                if *min_hops == 0 {
                    results.push((s, 0.0));
                }
            }
            while let Some((u, depth)) = queue.pop_front() {
                ctx.check_cancel()?;
                if depth >= *max_hops {
                    continue;
                }
                ctx.recorder.record_edge_adj(u, rel_kinds, *dir);
                let mut nexts = Vec::new();
                if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
                    for (dst, _, _) in scan_adj_out_raw(ctx.txn, &ctx.dbs, u, rel_kinds)? {
                        nexts.push(dst);
                    }
                }
                if matches!(dir, EdgeDir::In | EdgeDir::Both) {
                    for (src, _, _) in scan_adj_in_raw(ctx.txn, &ctx.dbs, u, rel_kinds)? {
                        nexts.push(src);
                    }
                }
                nexts.sort_unstable();
                nexts.dedup();
                for v in nexts {
                    if visited.contains_key(&v) || hidden.contains(&v) {
                        continue;
                    }
                    if visited.len().saturating_add(hidden.len()) >= ctx.budgets.max_traversal_visited {
                        return Err(ElmdbError::new(
                            ErrorCode::ResourceLimit,
                            "traversal exceeded max_traversal_visited budget",
                        ));
                    }
                    if !is_entity_visible(ctx, v, include_trashed)? {
                        ctx.charge_heap(16)?;
                        hidden.insert(v);
                        continue;
                    }
                    let nd = depth + 1;
                    visited.insert(v, nd);
                    ctx.charge_heap(32)?;
                    if nd >= *min_hops {
                        results.push((v, nd as f64));
                    }
                    if nd < *max_hops {
                        queue.push_back((v, nd));
                    }
                }
            }
            Ok(VecStreamOp::new(results))
        }
        TraverseSpec::ShortestPath { rel_kinds, dir, target, max_hops } => {
            if !is_entity_visible(ctx, *target, include_trashed)? {
                return Ok(VecStreamOp::new(Vec::new()));
            }
            let mut parent: FxHashMap<EntityKey, Option<EntityKey>> = FxHashMap::default();
            let mut hidden: FxHashSet<EntityKey> = FxHashSet::default();
            let mut queue: VecDeque<(EntityKey, u32)> = VecDeque::new();
            for s in seeds {
                ctx.check_cancel()?;
                if parent.contains_key(&s) || hidden.contains(&s) {
                    continue;
                }
                if !is_entity_visible(ctx, s, include_trashed)? {
                    if parent.len().saturating_add(hidden.len()) >= ctx.budgets.max_traversal_visited {
                        return Err(ElmdbError::new(
                            ErrorCode::ResourceLimit,
                            "shortest-path exceeded max_traversal_visited budget",
                        ));
                    }
                    ctx.charge_heap(16)?;
                    hidden.insert(s);
                    continue;
                }
                if s == *target {
                    return Ok(VecStreamOp::new(vec![(s, 0.0)]));
                }
                if parent.len().saturating_add(hidden.len()) >= ctx.budgets.max_traversal_visited {
                    return Err(ElmdbError::new(
                        ErrorCode::ResourceLimit,
                        "shortest-path exceeded max_traversal_visited budget",
                    ));
                }
                parent.insert(s, None);
                ctx.charge_heap(32)?;
                queue.push_back((s, 0));
            }
            let mut found = false;
            while let Some((u, depth)) = queue.pop_front() {
                ctx.check_cancel()?;
                if depth >= *max_hops {
                    continue;
                }
                ctx.recorder.record_edge_adj(u, rel_kinds, *dir);
                let mut nexts = Vec::new();
                if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
                    for (dst, _, _) in scan_adj_out_raw(ctx.txn, &ctx.dbs, u, rel_kinds)? {
                        nexts.push(dst);
                    }
                }
                if matches!(dir, EdgeDir::In | EdgeDir::Both) {
                    for (src, _, _) in scan_adj_in_raw(ctx.txn, &ctx.dbs, u, rel_kinds)? {
                        nexts.push(src);
                    }
                }
                nexts.sort_unstable();
                nexts.dedup();
                for v in nexts {
                    if parent.contains_key(&v) || hidden.contains(&v) {
                        continue;
                    }
                    if parent.len().saturating_add(hidden.len()) >= ctx.budgets.max_traversal_visited {
                        return Err(ElmdbError::new(
                            ErrorCode::ResourceLimit,
                            "shortest-path exceeded max_traversal_visited budget",
                        ));
                    }
                    if !is_entity_visible(ctx, v, include_trashed)? {
                        ctx.charge_heap(16)?;
                        hidden.insert(v);
                        continue;
                    }
                    parent.insert(v, Some(u));
                    ctx.charge_heap(32)?;
                    if v == *target {
                        found = true;
                        break;
                    }
                    queue.push_back((v, depth + 1));
                }
                if found {
                    break;
                }
            }
            if !found {
                return Ok(VecStreamOp::new(Vec::new()));
            }
            let mut path = Vec::new();
            let mut cur = Some(*target);
            while let Some(k) = cur {
                path.push(k);
                cur = parent.get(&k).copied().flatten();
            }
            path.reverse();
            let out: Vec<(EntityKey, f64)> = path.into_iter().enumerate().map(|(i, k)| (k, i as f64)).collect();
            Ok(VecStreamOp::new(out))
        }
    }
}

fn validate_cursor_matches_order(order: &OrderSpec, after: Option<&Cursor>) -> Result<()> {
    let Some(cursor) = after else {
        return Ok(());
    };
    let ok = matches!(
        (order, cursor),
        (OrderSpec::EntityKey(_), Cursor::EntityKey(_))
            | (OrderSpec::Score(_), Cursor::Score { .. })
            | (OrderSpec::Field { .. }, Cursor::Field { .. })
    );
    if !ok {
        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "pagination cursor variant does not match OrderSpec"));
    }
    Ok(())
}

fn eval_order_slice<R: KvRead>(
    ctx: &mut ExecCtx<'_, R>,
    input: &LogicalNode,
    order: &OrderSpec,
    after: Option<&Cursor>,
    limit: Option<u64>,
) -> Result<VecStreamOp> {
    validate_cursor_matches_order(order, after)?;
    let cap = limit.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    if cap == 0 {
        return Ok(VecStreamOp::new(Vec::new()));
    }

    // Fast path #1: Direct Order/Value index scan on unfiltered single-type live Source with no cursor!
    if after.is_none()
        && let LogicalNode::Source(SourceSpec::Type { type_id, include_trashed: false }) = input
        && let OrderSpec::Field { field, dir, locale } = order
    {
        let reverse = *dir == OrderDir::Desc;
        if let Some(loc) = locale {
            if let Some(idx) = find_ready_order_index(ctx.txn, &ctx.dbs, ctx.schema, *type_id, *field, loc)?
                && let Some(db) = ctx.open_index_ro(idx.id)?
            {
                ctx.check_cancel()?;
                // Record BOTH TAG_ORDER (0x01) and TAG_MISSING (0x02)!
                ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x03]);
                let cached = ctx.collators.get_or_build(loc)?;
                // Missing/Null sorts first in Asc (`type_rank(Null) == 0`) and last in Desc!
                let mut keys = Vec::new();
                if !reverse {
                    keys.extend(order::scan_missing_keys(ctx.txn, db, &cached, false, Some(cap))?);
                    let rem = cap.saturating_sub(keys.len());
                    keys.extend(order::scan_ordered(ctx.txn, &ctx.dbs, db, &cached, *field, false, Some(rem))?);
                } else {
                    keys.extend(order::scan_ordered(ctx.txn, &ctx.dbs, db, &cached, *field, true, Some(cap))?);
                    let rem = cap.saturating_sub(keys.len());
                    keys.extend(order::scan_missing_keys(ctx.txn, db, &cached, true, Some(rem))?);
                }
                ctx.charge_heap(keys.len().saturating_mul(16))?;
                return Ok(VecStreamOp::new(keys.into_iter().map(|k| (k, 0.0)).collect()));
            }
        } else if let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, *type_id, *field)?
            && matches!(&idx.kind, IndexKind::Value { fields, .. } if fields.len() == 1)
            && let Some(db) = ctx.open_index_ro(idx.id)?
        {
            ctx.check_cancel()?;
            // Record ALL 4 partitions (`0x01..0x05`) so inserting a long/null/unordered value invalidates!
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x05]);
            if value::scan_long_keys(ctx.txn, db)?.is_empty()
                && value::scan_without_keys(ctx.txn, db)?.is_empty()
                && value::scan_unordered_keys(ctx.txn, db)?.is_empty()
            {
                let keys = value::scan_range(ctx.txn, db, None, None, reverse, Some(cap))?;
                ctx.charge_heap(keys.len().saturating_mul(16))?;
                return Ok(VecStreamOp::new(keys.into_iter().map(|k| (k, 0.0)).collect()));
            }
        }
    }

    // Fast path #2: Push `limit` directly into `Score` ONLY when `input` is a `Score` node
    // (which produces descending-score output) and ordering by `Score(Desc)` with no cursor!
    let score_hint = if after.is_none()
        && matches!(order, OrderSpec::Score(OrderDir::Desc))
        && matches!(input, LogicalNode::Score { .. })
    {
        limit
    } else {
        None
    };

    let stream = eval_stream(ctx, input, score_hint)?;
    let mut pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;

    match order {
        OrderSpec::EntityKey(dir) => {
            pairs.sort_by(|a, b| match dir {
                OrderDir::Asc => a.0.cmp(&b.0),
                OrderDir::Desc => b.0.cmp(&a.0),
            });
            if let Some(Cursor::EntityKey(ck)) = after {
                pairs.retain(|(k, _)| match dir {
                    OrderDir::Asc => k > ck,
                    OrderDir::Desc => k < ck,
                });
            }
        }
        OrderSpec::Score(dir) => {
            pairs.sort_by(|a, b| match dir {
                OrderDir::Desc => b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)),
                OrderDir::Asc => a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)),
            });
            if let Some(Cursor::Score { score: cs, key: ck }) = after {
                pairs.retain(|(k, s)| match dir {
                    OrderDir::Desc => {
                        s.total_cmp(cs) == Ordering::Less || (s.total_cmp(cs) == Ordering::Equal && k > ck)
                    }
                    OrderDir::Asc => {
                        s.total_cmp(cs) == Ordering::Greater || (s.total_cmp(cs) == Ordering::Equal && k > ck)
                    }
                });
            }
        }
        OrderSpec::Field { field, dir, locale } => {
            let coll = match locale {
                Some(loc) => Some(ctx.collators.get_or_build(loc)?),
                None => None,
            };
            let mut fmask = FieldMask::empty();
            fmask.insert(*field);
            let mut keyed: Vec<(EntityKey, f64, CanonicalValue)> = Vec::with_capacity(pairs.len());
            for (ek, sc) in pairs {
                ctx.check_cancel()?;
                ctx.recorder.record_point_entity(ek, &fmask);
                let val = ctx.load_row(ek)?.and_then(|r| r.fields.get(field).cloned()).unwrap_or(CanonicalValue::Null);
                ctx.charge_heap(24 + estimate_value_bytes(&val))?;
                keyed.push((ek, sc, val));
            }
            keyed.sort_by(|a, b| {
                let ord = compare_with_collator(&a.2, &b.2, coll.as_deref());
                let ord = if *dir == OrderDir::Desc { ord.reverse() } else { ord };
                ord.then_with(|| if *dir == OrderDir::Desc { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) })
            });
            if let Some(Cursor::Field { value: cv, key: ck }) = after {
                keyed.retain(|(k, _, v)| {
                    let ord = compare_with_collator(v, cv, coll.as_deref());
                    let ord = if *dir == OrderDir::Desc { ord.reverse() } else { ord };
                    let ord = ord.then_with(|| if *dir == OrderDir::Desc { ck.cmp(k) } else { k.cmp(ck) });
                    ord == Ordering::Greater
                });
            }
            pairs = keyed.into_iter().map(|(k, s, _)| (k, s)).collect();
        }
    }

    pairs.truncate(cap);
    Ok(VecStreamOp::new(pairs))
}

// ---------------------------------------------------------------------------
// Canonical value comparison & hashing helpers
// ---------------------------------------------------------------------------

fn values_equal(a: &CanonicalValue, b: &CanonicalValue) -> bool {
    if let Some(ord) = compare_numeric(a, b) {
        return ord == Ordering::Equal;
    }
    a == b
}

fn compare_values(a: &CanonicalValue, b: &CanonicalValue) -> Option<Ordering> {
    if let Some(ord) = compare_numeric(a, b) {
        return Some(ord);
    }
    match (a, b) {
        (CanonicalValue::Bool(x), CanonicalValue::Bool(y)) => Some(x.cmp(y)),
        (CanonicalValue::Timestamp { ms: m1, ns: n1 }, CanonicalValue::Timestamp { ms: m2, ns: n2 }) => {
            Some(m1.cmp(m2).then_with(|| n1.cmp(n2)))
        }
        (CanonicalValue::EntityRef(x), CanonicalValue::EntityRef(y)) => Some(x.cmp(y)),
        (CanonicalValue::String(x), CanonicalValue::String(y)) => Some(x.cmp(y)),
        (CanonicalValue::DanglingRef(x), CanonicalValue::DanglingRef(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn type_rank(v: &CanonicalValue) -> u8 {
    match v {
        CanonicalValue::Null => 0,
        CanonicalValue::Bool(_) => 1,
        CanonicalValue::Int(_) | CanonicalValue::Float(_) => 2,
        CanonicalValue::Timestamp { .. } => 3,
        CanonicalValue::EntityRef(_) => 4,
        CanonicalValue::String(_) => 5,
        CanonicalValue::DanglingRef(_) => 6,
        CanonicalValue::Bytes(_) => 7,
        CanonicalValue::Array(_) => 8,
        CanonicalValue::Object(_) => 9,
    }
}

fn compare_values_total(a: &CanonicalValue, b: &CanonicalValue) -> Ordering {
    if let Some(ord) = compare_values(a, b) {
        return ord;
    }
    type_rank(a).cmp(&type_rank(b)).then_with(|| match (a, b) {
        (CanonicalValue::Float(x), CanonicalValue::Float(y)) => x.total_cmp(y),
        (CanonicalValue::Int(_), CanonicalValue::Float(y)) => {
            if y.is_nan() {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }
        (CanonicalValue::Float(x), CanonicalValue::Int(_)) => {
            if x.is_nan() {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        (CanonicalValue::Bytes(x), CanonicalValue::Bytes(y)) => x.cmp(y),
        (CanonicalValue::Array(xs), CanonicalValue::Array(ys)) => {
            for (x, y) in xs.iter().zip(ys.iter()) {
                let ord = compare_values_total(x, y);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            xs.len().cmp(&ys.len())
        }
        (CanonicalValue::Object(xs), CanonicalValue::Object(ys)) => {
            for ((kx, vx), (ky, vy)) in xs.iter().zip(ys.iter()) {
                let ord = kx.cmp(ky).then_with(|| compare_values_total(vx, vy));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            xs.len().cmp(&ys.len())
        }
        _ => Ordering::Equal,
    })
}

fn canonical_group_key(val: &CanonicalValue) -> Vec<u8> {
    let mut out = Vec::new();
    match value::encode_compound_values(&[val]) {
        ValueKeyClass::Indexed(bytes) => {
            out.push(1);
            out.extend_from_slice(&bytes);
        }
        _ => {
            out.push(2);
            elmdb_wire::encode_value(val, &mut out);
        }
    }
    out
}

fn compare_with_collator(
    a: &CanonicalValue,
    b: &CanonicalValue,
    coll: Option<&elmdb_projections::CachedCollator>,
) -> Ordering {
    if let Some(c) = coll
        && let (CanonicalValue::String(sa), CanonicalValue::String(sb)) = (a, b)
    {
        return c.collator.compare(sa, sb);
    }
    compare_values_total(a, b)
}

/// Stable 64-bit value hash for Tier-1 `IndexedEq(IndexId, ValueHash64)` read tracking!
/// Uses `encode_compound_values` so `Int(5)` and `Float(5.0)` hash identically!
pub fn hash_canonical_value(val: &CanonicalValue) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    };
    match value::encode_compound_values(&[val]) {
        ValueKeyClass::Indexed(bytes) => {
            mix(&[1]);
            mix(&bytes);
        }
        _ => {
            let mut enc = Vec::new();
            elmdb_wire::encode_value(val, &mut enc);
            mix(&[2]);
            mix(&enc);
        }
    }
    h
}
