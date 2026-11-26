//! Vectorized 256-key batched pull executor (`KeyBatch`), `ExecCtx` hard resource & cancellation
//! budgets, `QueryReadRecorder` seam for Phase 3b live-query invalidation, and `QueryEngine`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};

use elmdb_engine::{
    CompiledSchema, Engine, EntityKey, FieldId, FieldMask, IndexId, IndexKind, PackedRow, RelKindId, SystemDbs, TypeId,
    read_packed_row, scan_adj_in_raw, scan_adj_out_raw, scan_type_keys,
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
    CsrGraph, ExactSum, PprResult, TopoResult, adamic_adar_similar, jaccard_similar, leiden_communities, ppr, scc,
    topo_sort, weak_components,
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

/// Typed result produced by executing a `LogicalPlan`.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Ids(Vec<EntityKey>),
    Rows(Vec<ProjectedRow>),
    Hits(Vec<(EntityKey, f64)>),
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
    fn next_batch<'t, S: KvStore + 't, V: Vfs, C: Clock>(
        &mut self,
        ctx: &mut ExecCtx<'_, 't, S, V, C>,
        demand: u16,
        out: &mut KeyBatch,
    ) -> Result<bool>;
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
    fn next_batch<'t, S: KvStore + 't, V: Vfs, C: Clock>(
        &mut self,
        ctx: &mut ExecCtx<'_, 't, S, V, C>,
        demand: u16,
        out: &mut KeyBatch,
    ) -> Result<bool> {
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

/// Execution context carrying MVCC RO snapshot, pinned schema, budgets, read recorder, and `EXPLAIN`.
struct ExecCtx<'a, 't, S: KvStore + 't, V: Vfs, C: Clock> {
    engine: &'a Engine<S, V, C>,
    txn: &'a S::RoTxn<'t>,
    schema: &'a CompiledSchema,
    dbs: SystemDbs,
    collators: &'a CollatorCache,
    budgets: ExecBudgets<'a>,
    recorder: &'a mut dyn QueryReadRecorder,
    explain: ExplainPlan,
    heap_used: usize,
    row_cache: FxHashMap<EntityKey, PackedRow>,
}

impl<'a, 't, S: KvStore + 't, V: Vfs, C: Clock> ExecCtx<'a, 't, S, V, C> {
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

    fn open_index_ro(&self, index_id: IndexId) -> Result<Option<SubDbHandle>> {
        let def = self
            .schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        let handle = self.engine.gate().open_subdb_ro(self.txn, &def.subdb_slot().name(), DbFlags::DEFAULT)?;
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
        // Keep bounded row cache so repeated filter/order/project probes don't re-decode blobs.
        if self.row_cache.len() < 4096 {
            self.row_cache.insert(key, row.clone());
        }
        Ok(Some(row))
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

    /// Executes `plan` inside an already-open MVCC `RoTxn` (used by tracked watches and `transact`).
    pub fn execute_in_txn<'t, S: KvStore + 't, V: Vfs, C: Clock>(
        &self,
        engine: &Engine<S, V, C>,
        txn: &S::RoTxn<'t>,
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

        let mut ctx = ExecCtx {
            engine,
            txn,
            schema,
            dbs,
            collators: &self.collators,
            budgets,
            recorder,
            explain,
            heap_used: 0,
            row_cache: FxHashMap::default(),
        };

        // Fast path: O(1) exact Count pushdown without reading a single row!
        if strategy.use_count_pushdown
            && let LogicalNode::Terminal { input, terminal: TerminalSpec::Count } = &plan.root
            && let LogicalNode::Filter { expr: FilterExpr::Eq(_, val), .. } = input.as_ref()
            && let Some(idx_id) = strategy.chosen_index
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

fn eval_root<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    node: &LogicalNode,
) -> Result<QueryResult> {
    match node {
        LogicalNode::GraphAnalyze { input, spec } => eval_graph_analyze(ctx, input, spec),
        LogicalNode::Terminal { input, terminal } => {
            // If input is GraphAnalyze directly under Terminal, let GraphAnalyze produce its structured result
            // unless terminal is Ids/Count/Exists/Hits!
            if let LogicalNode::GraphAnalyze { input: g_in, spec } = input.as_ref() {
                let gres = eval_graph_analyze(ctx, g_in, spec)?;
                return project_graph_terminal(gres, terminal);
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

fn project_graph_terminal(gres: QueryResult, terminal: &TerminalSpec) -> Result<QueryResult> {
    match (gres, terminal) {
        (QueryResult::GraphPpr(ppr), TerminalSpec::Hits) => Ok(QueryResult::Hits(ppr.scores)),
        (QueryResult::GraphPpr(ppr), TerminalSpec::Ids) => {
            Ok(QueryResult::Ids(ppr.scores.into_iter().map(|(k, _)| k).collect()))
        }
        (QueryResult::GraphTopo(topo), TerminalSpec::Ids) => Ok(QueryResult::Ids(topo.order)),
        (QueryResult::Hits(h), TerminalSpec::Ids) => Ok(QueryResult::Ids(h.into_iter().map(|(k, _)| k).collect())),
        (other, _) => Ok(other),
    }
}

fn eval_graph_analyze<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    input: &LogicalNode,
    spec: &GraphAnalyzeSpec,
) -> Result<QueryResult> {
    let stream = eval_stream(ctx, input, None)?;
    let pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;
    let nodes: Vec<EntityKey> = pairs.into_iter().map(|(k, _)| k).collect();

    match spec {
        GraphAnalyzeSpec::WeakComponents { rel_kinds } => {
            let g = CsrGraph::build(ctx.txn, &ctx.dbs, nodes, rel_kinds, false, ctx.budgets.max_heap_bytes)?;
            let comps = weak_components(&g);
            ctx.explain.rows_returned = comps.len() as u64;
            Ok(QueryResult::GraphGroups(comps))
        }
        GraphAnalyzeSpec::Scc { rel_kinds } => {
            let g = CsrGraph::build(ctx.txn, &ctx.dbs, nodes, rel_kinds, false, ctx.budgets.max_heap_bytes)?;
            let comps = scc(&g);
            ctx.explain.rows_returned = comps.len() as u64;
            Ok(QueryResult::GraphGroups(comps))
        }
        GraphAnalyzeSpec::TopoSort { rel_kinds } => {
            let g = CsrGraph::build(ctx.txn, &ctx.dbs, nodes, rel_kinds, false, ctx.budgets.max_heap_bytes)?;
            let res = topo_sort(&g);
            ctx.explain.rows_returned = res.order.len() as u64;
            Ok(QueryResult::GraphTopo(res))
        }
        GraphAnalyzeSpec::Ppr { rel_kinds, weighted, seeds, damping, iterations, tolerance } => {
            let g = CsrGraph::build(ctx.txn, &ctx.dbs, nodes, rel_kinds, *weighted, ctx.budgets.max_heap_bytes)?;
            let res = ppr(&g, seeds, *damping, *iterations, *tolerance)?;
            ctx.explain.rows_returned = res.scores.len() as u64;
            Ok(QueryResult::GraphPpr(res))
        }
        GraphAnalyzeSpec::Leiden { rel_kinds, weighted, resolution, seed, iterations } => {
            let g = CsrGraph::build(ctx.txn, &ctx.dbs, nodes, rel_kinds, *weighted, ctx.budgets.max_heap_bytes)?;
            let groups = leiden_communities(&g, *resolution, *seed, *iterations)?;
            ctx.explain.rows_returned = groups.len() as u64;
            Ok(QueryResult::GraphGroups(groups))
        }
        GraphAnalyzeSpec::Jaccard { rel_kinds, dir, seed, top_k } => {
            let cand: FxHashSet<EntityKey> = nodes.into_iter().collect();
            let hits = jaccard_similar(ctx.txn, &ctx.dbs, Some(&cand), rel_kinds, *dir, *seed, *top_k)?;
            ctx.explain.rows_returned = hits.len() as u64;
            Ok(QueryResult::Hits(hits))
        }
        GraphAnalyzeSpec::AdamicAdar { rel_kinds, dir, seed, top_k } => {
            let cand: FxHashSet<EntityKey> = nodes.into_iter().collect();
            let hits = adamic_adar_similar(ctx.txn, &ctx.dbs, Some(&cand), rel_kinds, *dir, *seed, *top_k)?;
            ctx.explain.rows_returned = hits.len() as u64;
            Ok(QueryResult::Hits(hits))
        }
    }
}

fn eval_terminal<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
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
        TerminalSpec::Hits => {
            let stream = eval_stream(ctx, input, None)?;
            let pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;
            ctx.explain.rows_returned = pairs.len() as u64;
            Ok(QueryResult::Hits(pairs))
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
                        let mut mask = FieldMask::empty();
                        match fields {
                            Some(fm) => {
                                for (&fid, val) in &row.fields {
                                    if fm.contains(fid) {
                                        proj_fields.insert(fid, val.clone());
                                        mask.insert(fid);
                                    }
                                }
                            }
                            None => {
                                for (&fid, val) in &row.fields {
                                    proj_fields.insert(fid, val.clone());
                                    mask.insert(fid);
                                }
                            }
                        }
                        ctx.recorder.record_point_entity(ek, &mask);
                        ctx.charge_heap(64 + proj_fields.len() * 32)?;
                        let trashed = row.header.is_trashed();
                        rows.push(ProjectedRow {
                            key: ek,
                            id: row.header.ext_id,
                            type_id: row.header.type_id,
                            trashed,
                            fields: proj_fields,
                        });
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
                        if min_v.as_ref().is_none_or(|cur| compare_values(val, cur) == Some(Ordering::Less)) {
                            min_v = Some(val.clone());
                        }
                        if max_v.as_ref().is_none_or(|cur| compare_values(val, cur) == Some(Ordering::Greater)) {
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
                let mut counts: Vec<(CanonicalValue, u64)> = Vec::new();
                while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                    for i in 0..batch.len as usize {
                        let ek = batch.keys[i];
                        ctx.recorder.record_point_entity(ek, &fmask);
                        let key_val = ctx
                            .load_row(ek)?
                            .and_then(|r| r.fields.get(field).cloned())
                            .unwrap_or(CanonicalValue::Null);
                        if let Some((_, c)) = counts.iter_mut().find(|(k, _)| values_equal(k, &key_val)) {
                            *c += 1;
                        } else {
                            ctx.charge_heap(48)?;
                            counts.push((key_val, 1));
                        }
                    }
                }
                counts.sort_by(|a, b| compare_values_total(&a.0, &b.0));
                ctx.explain.rows_returned = counts.len() as u64;
                Ok(QueryResult::GroupCounts(counts))
            } else {
                let mut groups: Vec<(CanonicalValue, Vec<EntityKey>)> = Vec::new();
                while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                    for i in 0..batch.len as usize {
                        let ek = batch.keys[i];
                        ctx.recorder.record_point_entity(ek, &fmask);
                        let key_val = ctx
                            .load_row(ek)?
                            .and_then(|r| r.fields.get(field).cloned())
                            .unwrap_or(CanonicalValue::Null);
                        if let Some((_, members)) = groups.iter_mut().find(|(k, _)| values_equal(k, &key_val)) {
                            ctx.charge_heap(8)?;
                            members.push(ek);
                        } else {
                            ctx.charge_heap(64)?;
                            groups.push((key_val, vec![ek]));
                        }
                    }
                }
                groups.sort_by(|a, b| compare_values_total(&a.0, &b.0));
                ctx.explain.rows_returned = groups.len() as u64;
                Ok(QueryResult::Groups(groups))
            }
        }
        TerminalSpec::Degrees { rel_kinds, dir } => {
            let mut stream = eval_stream(ctx, input, None)?;
            let mut batch = KeyBatch::default();
            let mut out = Vec::new();
            while stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
                for i in 0..batch.len as usize {
                    let ek = batch.keys[i];
                    ctx.recorder.record_edge_adj(ek, rel_kinds, *dir);
                    let mut deg = 0u64;
                    if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
                        deg += scan_adj_out_raw(ctx.txn, &ctx.dbs, ek, rel_kinds)?.len() as u64;
                    }
                    if matches!(dir, EdgeDir::In | EdgeDir::Both) {
                        deg += scan_adj_in_raw(ctx.txn, &ctx.dbs, ek, rel_kinds)?.len() as u64;
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

fn collect_all_pairs<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
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
fn eval_stream<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
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

fn eval_source<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    src: &SourceSpec,
    limit_hint: Option<u64>,
) -> Result<VecStreamOp> {
    let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    let mut out = Vec::new();
    match src {
        SourceSpec::Type { type_id, include_trashed } => {
            ctx.recorder.record_type_scan(*type_id);
            let keys = scan_type_keys(ctx.txn, &ctx.dbs, *type_id)?;
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
                    break;
                }
            }
        }
        SourceSpec::Types { type_ids, include_trashed } => {
            for &tid in type_ids {
                ctx.recorder.record_type_scan(tid);
                let keys = scan_type_keys(ctx.txn, &ctx.dbs, tid)?;
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
            }
            out.sort_by_key(|(k, _)| *k);
            out.dedup_by_key(|(k, _)| *k);
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
            for &ek in keys {
                ctx.check_cancel()?;
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

fn eval_filter<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
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
            if let Some((keys, needs_residual, label)) = try_pushdown_conjunct(ctx, *type_id, &c)? {
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
            let candidates = leapfrog_intersect(index_lists);
            ctx.explain.residual_filters += residual.len();
            if residual.is_empty() {
                let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
                let pairs: Vec<(EntityKey, f64)> = candidates.into_iter().take(cap).map(|k| (k, 0.0)).collect();
                return Ok(VecStreamOp::new(pairs));
            }
            let res_expr = if residual.len() == 1 { residual.pop().unwrap() } else { FilterExpr::And(residual) };
            let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
            let mut out = Vec::new();
            for ek in candidates {
                ctx.check_cancel()?;
                if eval_predicate(ctx, ek, &res_expr)? {
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

    // Also push trigram narrowing when Filter(Contains/Regex) sits above an arbitrary input on a known type!
    let mut input_stream = eval_stream(ctx, input, None)?;
    ctx.explain.residual_filters += 1;
    let cap = limit_hint.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    let mut batch = KeyBatch::default();
    let mut out = Vec::new();
    while input_stream.next_batch(ctx, BATCH_SIZE as u16, &mut batch)? {
        for i in 0..batch.len as usize {
            let ek = batch.keys[i];
            if eval_predicate(ctx, ek, expr)? {
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
fn try_pushdown_conjunct<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
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
            ctx.recorder.record_indexed_eq(idx.id, hash_canonical_value(val));
            match value::encode_compound_values(&[val]) {
                ValueKeyClass::Indexed(_) => {
                    // For single-field index: exact lookup_eq; for compound leading-field index: prefix scan_range!
                    let is_single = matches!(&idx.kind, IndexKind::Value { fields, .. } if fields.len() == 1);
                    let mut keys = if is_single {
                        value::lookup_eq(ctx.txn, db, &[val])?
                    } else {
                        value::scan_range(ctx.txn, db, Some((&[val], true)), Some((&[val], true)), false, None)?
                    };
                    // If `val` is a string, also check `TAG_LONG` (only non-empty when > 448 B strings exist, or compound > 502 B)!
                    let long_keys = value::scan_long_keys(ctx.txn, db)?;
                    let needs_residual = !long_keys.is_empty();
                    keys.extend(long_keys);
                    Ok(Some((keys, needs_residual, format!("ValueEq({})", idx.name))))
                }
                ValueKeyClass::Long => {
                    // Long string > 448 B lives exclusively in `TAG_LONG`!
                    let long_keys = value::scan_long_keys(ctx.txn, db)?;
                    Ok(Some((long_keys, true, format!("ValueLongEq({})", idx.name))))
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
                ctx.recorder.record_indexed_eq(idx.id, hash_canonical_value(v));
                match value::encode_compound_values(&[v]) {
                    ValueKeyClass::Indexed(_) => {
                        if is_single {
                            keys.extend(value::lookup_eq(ctx.txn, db, &[v])?);
                        } else {
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
            let long_keys = value::scan_long_keys(ctx.txn, db)?;
            let needs_residual = has_long || !long_keys.is_empty();
            keys.extend(long_keys);
            Ok(Some((keys, needs_residual, format!("ValueIn({})", idx.name))))
        }
        FilterExpr::Range { field, low, high } => {
            let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
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
            let mut keys = value::scan_range(ctx.txn, db, lo, hi, false, None)?;
            let long_keys = value::scan_long_keys(ctx.txn, db)?;
            let needs_residual = !long_keys.is_empty();
            keys.extend(long_keys);
            Ok(Some((keys, needs_residual, format!("ValueRange({})", idx.name))))
        }
        FilterExpr::StartsWith(field, prefix) => {
            let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, type_id, *field)? else {
                return Ok(None);
            };
            let Some(db) = ctx.open_index_ro(idx.id)? else {
                return Ok(None);
            };
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
            let mut keys = value::scan_prefix_string(ctx.txn, db, prefix, None)?;
            let long_keys = value::scan_long_keys(ctx.txn, db)?;
            let needs_residual = !long_keys.is_empty();
            keys.extend(long_keys);
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
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
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
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
            let keys = trigram::intersect_trigrams(ctx.txn, db, &trigs)?;
            Ok(Some((keys, true, format!("TrigramRegex({})", idx.name))))
        }
        FilterExpr::HasLink { rel_kinds, dir, target: Some(target_key) } => {
            // Drive-from-smaller-side for `HasLink { target: X }`: look up reverse adjacency of `target_key`!
            let rev_dir = match dir {
                EdgeDir::Out => EdgeDir::In,
                EdgeDir::In => EdgeDir::Out,
                EdgeDir::Both => EdgeDir::Both,
            };
            ctx.recorder.record_edge_adj(*target_key, rel_kinds, rev_dir);
            let mut linked = Vec::new();
            if matches!(rev_dir, EdgeDir::In | EdgeDir::Both) {
                for (src, _, _) in scan_adj_in_raw(ctx.txn, &ctx.dbs, *target_key, rel_kinds)? {
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

fn eval_predicate<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    key: EntityKey,
    expr: &FilterExpr,
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
        FilterExpr::Ne(field, target) => Ok(!eval_predicate(ctx, key, &FilterExpr::Eq(*field, target.clone()))?),
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
            let low_needle = needle.to_lowercase();
            match row.fields.get(field) {
                Some(CanonicalValue::String(s)) => Ok(s.to_lowercase().contains(&low_needle)),
                Some(CanonicalValue::Array(items)) => Ok(items
                    .iter()
                    .any(|it| matches!(it, CanonicalValue::String(s) if s.to_lowercase().contains(&low_needle)))),
                _ => Ok(false),
            }
        }
        FilterExpr::Regex(field, pattern) => {
            let re = Regex::new(pattern)
                .map_err(|e| ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid regex '{pattern}': {e}")))?;
            let mut m = FieldMask::empty();
            m.insert(*field);
            ctx.recorder.record_point_entity(key, &m);
            let Some(row) = ctx.load_row(key)? else { return Ok(false) };
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
        FilterExpr::IsPresent(field) => Ok(!eval_predicate(ctx, key, &FilterExpr::IsNull(*field))?),
        FilterExpr::HasLink { rel_kinds, dir, target } => {
            ctx.recorder.record_edge_adj(key, rel_kinds, *dir);
            if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
                let edges = scan_adj_out_raw(ctx.txn, &ctx.dbs, key, rel_kinds)?;
                if edges.iter().any(|(dst, _, _)| target.is_none_or(|t| t == *dst)) {
                    return Ok(true);
                }
            }
            if matches!(dir, EdgeDir::In | EdgeDir::Both) {
                let edges = scan_adj_in_raw(ctx.txn, &ctx.dbs, key, rel_kinds)?;
                if edges.iter().any(|(src, _, _)| target.is_none_or(|t| t == *src)) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        FilterExpr::And(parts) => {
            for p in parts {
                if !eval_predicate(ctx, key, p)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        FilterExpr::Or(parts) => {
            for p in parts {
                if eval_predicate(ctx, key, p)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        FilterExpr::Not(inner) => Ok(!eval_predicate(ctx, key, inner)?),
    }
}

fn eval_score<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    input: &LogicalNode,
    spec: &ScoreSpec,
    limit_hint: Option<u64>,
) -> Result<VecStreamOp> {
    // Determine whether input is an unfiltered single-type source (allowing direct index-driven BMW/HNSW/Rank)
    // or a filtered candidate set to push down into BMW/HNSW!
    let cand_set: Option<FxHashSet<EntityKey>> = match input {
        LogicalNode::Source(SourceSpec::Type { include_trashed: false, .. })
            if matches!(
                spec,
                ScoreSpec::Bm25f { .. }
                    | ScoreSpec::VectorNear { .. }
                    | ScoreSpec::VectorLike { .. }
                    | ScoreSpec::RankIndex { .. }
            ) =>
        {
            None
        }
        _ => {
            let stream = eval_stream(ctx, input, None)?;
            let pairs = collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?;
            Some(pairs.into_iter().map(|(k, _)| k).collect())
        }
    };

    let scored = eval_score_signal(ctx, input, spec, cand_set.as_ref(), limit_hint)?;
    Ok(VecStreamOp::new(scored))
}

fn eval_score_signal<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    input: &LogicalNode,
    spec: &ScoreSpec,
    candidates: Option<&FxHashSet<EntityKey>>,
    limit_hint: Option<u64>,
) -> Result<Vec<(EntityKey, f64)>> {
    match spec {
        ScoreSpec::Bm25f { index_id, query, require_all, phrase, top_k } => {
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "text index is not ready"));
            }
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            let def = ctx.schema.index_by_id(*index_id).unwrap();
            ctx.recorder.record_subdb_range(def.slot, &[0x01], &[0x05]);
            let k = top_k.or_else(|| limit_hint.map(|l| l.min(usize::MAX as u64) as usize)).unwrap_or(10_000);
            let fetch_k = if phrase.is_some() { k.saturating_mul(4).max(128) } else { k };
            let (mut hits, _) = text::search_bmw(ctx.txn, db, query, None, fetch_k, *require_all, candidates)?;
            if let Some(phrase_terms) = phrase {
                let mut verified = Vec::with_capacity(hits.len());
                for h in hits {
                    if text::matches_phrase(ctx.txn, db, h.key, phrase_terms)? {
                        verified.push(h);
                        if verified.len() >= k {
                            break;
                        }
                    }
                }
                hits = verified;
            }
            Ok(hits.into_iter().map(|h| (h.key, h.score)).collect())
        }
        ScoreSpec::VectorNear { index_id, vector, top_k } => {
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "vector index is not ready"));
            }
            let def = ctx
                .schema
                .index_by_id(*index_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", index_id.0)))?;
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            ctx.recorder.record_subdb_range(def.slot, &[0x01], &[0x04]);
            let k = (*top_k).min(limit_hint.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize));
            let hits = search_near(
                ctx.txn,
                &ctx.dbs,
                def,
                db,
                vector,
                SearchNearOpts { top_k: k, candidates, best_chunk_per_entity: true },
            )?;
            Ok(hits.into_iter().map(|h| (h.key, h.score as f64)).collect())
        }
        ScoreSpec::VectorLike { index_id, seed, top_k } => {
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "vector index is not ready"));
            }
            let def = ctx
                .schema
                .index_by_id(*index_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", index_id.0)))?;
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            ctx.recorder.record_subdb_range(def.slot, &[0x01], &[0x04]);
            let hits = search_like(ctx.txn, &ctx.dbs, def, db, *seed, *top_k, candidates)?;
            Ok(hits.into_iter().map(|h| (h.key, h.score as f64)).collect())
        }
        ScoreSpec::RankIndex { index_id } => {
            if !is_index_ready(ctx.txn, &ctx.dbs, *index_id)? {
                return Err(ElmdbError::new(ErrorCode::IndexNotReady, "rank index is not ready"));
            }
            let def = ctx
                .schema
                .index_by_id(*index_id)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index {}", index_id.0)))?;
            let Some(db) = ctx.open_index_ro(*index_id)? else {
                return Ok(Vec::new());
            };
            ctx.recorder.record_subdb_range(def.slot, &[0x01], &[0x03]);
            match candidates {
                None => {
                    let lim = limit_hint.map(|l| l.min(usize::MAX as u64) as usize);
                    rank::scan_ranked(ctx.txn, db, false, lim)
                }
                Some(allowed) => {
                    let mut out = Vec::with_capacity(allowed.len());
                    for &ek in allowed {
                        if let Some(score) = rank::rank_of(ctx.txn, db, ek)? {
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
                        let age = (*now_ms - ms).max(0) as f64;
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
            let mut fused: FxHashMap<EntityKey, f64> = FxHashMap::default();
            match *method {
                FuseMethod::Rrf { k } => {
                    if !k.is_finite() || k <= 0.0 {
                        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "RRF k must be > 0"));
                    }
                    for (sub_spec, weight) in inputs {
                        if !weight.is_finite() || *weight <= 0.0 {
                            continue;
                        }
                        let mut sub_hits = eval_score_signal(ctx, input, sub_spec, candidates, None)?;
                        sub_hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                        for (rank_0, (ek, _)) in sub_hits.into_iter().enumerate() {
                            let contrib = *weight / (k + (rank_0 as f64) + 1.0);
                            *fused.entry(ek).or_insert(0.0) += contrib;
                        }
                    }
                }
                FuseMethod::LinearNormalized => {
                    for (sub_spec, weight) in inputs {
                        if !weight.is_finite() || *weight <= 0.0 {
                            continue;
                        }
                        let sub_hits = eval_score_signal(ctx, input, sub_spec, candidates, None)?;
                        if sub_hits.is_empty() {
                            continue;
                        }
                        let mut min_s = f64::INFINITY;
                        let mut max_s = f64::NEG_INFINITY;
                        for &(_, s) in &sub_hits {
                            min_s = min_s.min(s);
                            max_s = max_s.max(s);
                        }
                        let span = max_s - min_s;
                        for (ek, s) in sub_hits {
                            // Guard `max == min`: constant positive pool normalizes to 1.0 (never divides by zero!).
                            let norm = if span.abs() <= f64::EPSILON {
                                if s > 0.0 { 1.0 } else { 0.0 }
                            } else {
                                ((s - min_s) / span).clamp(0.0, 1.0)
                            };
                            *fused.entry(ek).or_insert(0.0) += *weight * norm;
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

fn eval_traverse<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    input: &LogicalNode,
    spec: &TraverseSpec,
) -> Result<VecStreamOp> {
    let stream = eval_stream(ctx, input, None)?;
    let seeds: Vec<EntityKey> =
        collect_all_pairs(ctx, stream, BATCH_SIZE as u16)?.into_iter().map(|(k, _)| k).collect();

    match spec {
        TraverseSpec::Hops { rel_kinds, dir, min_hops, max_hops } => {
            let mut visited: FxHashMap<EntityKey, u32> = FxHashMap::default();
            let mut queue: VecDeque<(EntityKey, u32)> = VecDeque::new();
            let mut results = Vec::new();
            for s in seeds {
                if visited.insert(s, 0).is_none() {
                    queue.push_back((s, 0));
                    if *min_hops == 0 {
                        results.push((s, 0.0));
                    }
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
                    if !visited.contains_key(&v) {
                        if visited.len() >= ctx.budgets.max_traversal_visited {
                            return Err(ElmdbError::new(
                                ErrorCode::ResourceLimit,
                                "traversal exceeded max_traversal_visited budget",
                            ));
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
            }
            Ok(VecStreamOp::new(results))
        }
        TraverseSpec::ShortestPath { rel_kinds, dir, target, max_hops } => {
            let mut parent: FxHashMap<EntityKey, Option<EntityKey>> = FxHashMap::default();
            let mut queue: VecDeque<(EntityKey, u32)> = VecDeque::new();
            for s in seeds {
                if s == *target {
                    return Ok(VecStreamOp::new(vec![(s, 0.0)]));
                }
                if parent.insert(s, None).is_none() {
                    queue.push_back((s, 0));
                }
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
                    if !parent.contains_key(&v) {
                        if parent.len() >= ctx.budgets.max_traversal_visited {
                            return Err(ElmdbError::new(
                                ErrorCode::ResourceLimit,
                                "shortest-path exceeded max_traversal_visited budget",
                            ));
                        }
                        parent.insert(v, Some(u));
                        ctx.charge_heap(32)?;
                        if v == *target {
                            found = true;
                            break;
                        }
                        queue.push_back((v, depth + 1));
                    }
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

fn eval_order_slice<'t, S: KvStore + 't, V: Vfs, C: Clock>(
    ctx: &mut ExecCtx<'_, 't, S, V, C>,
    input: &LogicalNode,
    order: &OrderSpec,
    after: Option<&Cursor>,
    limit: Option<u64>,
) -> Result<VecStreamOp> {
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
                ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
                let cached = ctx.collators.get_or_build(loc)?;
                let keys = order::scan_ordered(ctx.txn, &ctx.dbs, db, &cached, *field, reverse, Some(cap))?;
                return Ok(VecStreamOp::new(keys.into_iter().map(|k| (k, 0.0)).collect()));
            }
        } else if let Some(idx) = find_ready_value_index(ctx.txn, &ctx.dbs, ctx.schema, *type_id, *field)?
            && matches!(&idx.kind, IndexKind::Value { fields, .. } if fields.len() == 1)
            && let Some(db) = ctx.open_index_ro(idx.id)?
            && value::scan_long_keys(ctx.txn, db)?.is_empty()
            && value::scan_without_keys(ctx.txn, db)?.is_empty()
            && value::scan_unordered_keys(ctx.txn, db)?.is_empty()
        {
            ctx.recorder.record_subdb_range(idx.slot, &[0x01], &[0x02]);
            let keys = value::scan_range(ctx.txn, db, None, None, reverse, Some(cap))?;
            return Ok(VecStreamOp::new(keys.into_iter().map(|k| (k, 0.0)).collect()));
        }
    }

    // Fast path #2: Push `limit` directly into `Score` when ordering by `Score(Desc)` with no cursor!
    let score_hint = if after.is_none() && matches!(order, OrderSpec::Score(OrderDir::Desc)) { limit } else { None };

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
                ctx.recorder.record_point_entity(ek, &fmask);
                let val = ctx.load_row(ek)?.and_then(|r| r.fields.get(field).cloned()).unwrap_or(CanonicalValue::Null);
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
        (CanonicalValue::Bytes(x), CanonicalValue::Bytes(y)) => x.cmp(y),
        _ => Ordering::Equal,
    })
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
