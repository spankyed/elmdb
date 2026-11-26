//! Rule & cost optimizer, regex-HIR trigram literal extraction, `Spread`-adaptive
//! multi-index Leapfrog intersection, and structured `ExplainPlan` telemetry.

use elmdb_engine::{CompiledSchema, EntityKey, FieldId, IndexDef, IndexId, IndexKind, SystemDbs, TypeId};
use elmdb_projections::{BuildState, state::read_build_state, trigram};
use elmdb_storage::KvRead;
use elmdb_wire::{CanonicalValue, Result};
use regex_syntax::hir::{Hir, HirKind};
use serde::{Deserialize, Serialize};

use crate::cache::{CachedStrategy, PlanCache, PlanShapeKey};
use crate::ir::{FilterExpr, LogicalNode, LogicalPlan, OrderSpec, SourceSpec, TerminalSpec};

/// Structured `EXPLAIN` output showing chosen physical access paths, pushed predicates,
/// plan-cache hit status, and execution counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainPlan {
    pub access_path: String,
    pub chosen_indexes: Vec<String>,
    pub pushed_filters: Vec<String>,
    pub residual_filters: usize,
    pub count_pushdown: bool,
    pub order_pushdown: Option<String>,
    pub plan_cache_hit: bool,
    pub batches_pulled: u64,
    pub rows_scanned: u64,
    pub rows_returned: u64,
}

/// Checks whether `index_id` is persisted in `BuildState::Ready`.
pub fn is_index_ready<R: KvRead>(txn: &R, dbs: &SystemDbs, index_id: IndexId) -> Result<bool> {
    Ok(matches!(read_build_state(txn, dbs, index_id)?, None | Some(BuildState::Ready)))
}

/// Finds a ready `Value` index on `(type_id, field)` whose leading field is `field`.
pub fn find_ready_value_index<'a, R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    schema: &'a CompiledSchema,
    type_id: TypeId,
    field: FieldId,
) -> Result<Option<&'a IndexDef>> {
    let mut fallback = None;
    for def in &schema.indexes {
        if def.type_id != type_id {
            continue;
        }
        if let IndexKind::Value { fields, .. } = &def.kind
            && fields.first() == Some(&field)
            && is_index_ready(txn, dbs, def.id)?
        {
            if fields.len() == 1 {
                return Ok(Some(def));
            }
            if fallback.is_none() {
                fallback = Some(def);
            }
        }
    }
    Ok(fallback)
}

/// Finds a ready `Order` index on `(type_id, field, locale)`.
pub fn find_ready_order_index<'a, R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    schema: &'a CompiledSchema,
    type_id: TypeId,
    field: FieldId,
    locale: &str,
) -> Result<Option<&'a IndexDef>> {
    for def in &schema.indexes {
        if def.type_id != type_id {
            continue;
        }
        if let IndexKind::Order { field: f, locale: loc } = &def.kind
            && *f == field
            && loc == locale
            && is_index_ready(txn, dbs, def.id)?
        {
            return Ok(Some(def));
        }
    }
    Ok(None)
}

/// Finds a ready `Trigram` index on `type_id` covering `field`.
pub fn find_ready_trigram_index<'a, R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    schema: &'a CompiledSchema,
    type_id: TypeId,
    field: FieldId,
) -> Result<Option<&'a IndexDef>> {
    for def in &schema.indexes {
        if def.type_id != type_id {
            continue;
        }
        if let IndexKind::Trigram { fields } = &def.kind
            && fields.contains(&field)
            && is_index_ready(txn, dbs, def.id)?
        {
            return Ok(Some(def));
        }
    }
    Ok(None)
}

/// Extracts required lowercase 3-char trigrams from a `regex` pattern via a single `regex_syntax::hir` parse
/// (including small `HirKind::Class` expansions up to 8 chars, matching v2 spec §5.2!).
pub fn extract_regex_trigrams(pattern: &str) -> Vec<String> {
    let Ok(hir) = regex_syntax::parse(pattern) else {
        return Vec::new();
    };
    let mut literals = Vec::new();
    collect_required_literals(&hir, &mut literals);
    let mut out = Vec::new();
    for lit in literals {
        out.extend(trigram::extract_trigrams(&lit));
    }
    out.sort();
    out.dedup();
    out
}

fn collect_required_literals(hir: &Hir, out: &mut Vec<String>) {
    match hir.kind() {
        HirKind::Literal(lit) => {
            if let Ok(s) = std::str::from_utf8(&lit.0) {
                out.push(s.to_owned());
            }
        }
        HirKind::Capture(cap) => collect_required_literals(&cap.sub, out),
        HirKind::Concat(subs) => {
            // Merge adjacent literal / small single-char class pieces into contiguous substrings!
            let mut runs: Vec<String> = vec![String::new()];
            for sub in subs {
                match sub.kind() {
                    HirKind::Literal(lit) => {
                        if let Ok(s) = std::str::from_utf8(&lit.0) {
                            for r in &mut runs {
                                r.push_str(s);
                            }
                        } else {
                            flush_runs(&mut runs, out);
                        }
                    }
                    HirKind::Class(cls) => {
                        if let Some(chars) = expand_small_class(cls, 8)
                            && runs.len().saturating_mul(chars.len()) <= 8
                        {
                            let mut next = Vec::with_capacity(runs.len() * chars.len());
                            for r in &runs {
                                for &c in &chars {
                                    let mut nr = r.clone();
                                    nr.push(c);
                                    next.push(nr);
                                }
                            }
                            runs = next;
                        } else {
                            flush_runs(&mut runs, out);
                        }
                    }
                    HirKind::Capture(cap) => {
                        flush_runs(&mut runs, out);
                        collect_required_literals(&cap.sub, out);
                    }
                    _ => flush_runs(&mut runs, out),
                }
            }
            flush_runs(&mut runs, out);
        }
        HirKind::Repetition(rep) if rep.min >= 1 => {
            collect_required_literals(&rep.sub, out);
        }
        _ => {}
    }
}

fn flush_runs(runs: &mut Vec<String>, out: &mut Vec<String>) {
    if runs.len() == 1 {
        if runs[0].chars().count() >= 3 {
            out.push(std::mem::take(&mut runs[0]));
        } else {
            runs[0].clear();
        }
    } else if runs.len() > 1 {
        // Take trigrams common to ALL expanded branches!
        let mut common = trigram::extract_trigrams(&runs[0]);
        for r in &runs[1..] {
            let other = trigram::extract_trigrams(r);
            common.retain(|t| other.binary_search(t).is_ok());
        }
        out.extend(common);
        runs.clear();
        runs.push(String::new());
    }
}

fn expand_small_class(cls: &regex_syntax::hir::Class, max_chars: usize) -> Option<Vec<char>> {
    let mut out = Vec::new();
    match cls {
        regex_syntax::hir::Class::Unicode(u) => {
            for r in u.ranges() {
                for c in r.start()..=r.end() {
                    out.push(c);
                    if out.len() > max_chars {
                        return None;
                    }
                }
            }
        }
        regex_syntax::hir::Class::Bytes(b) => {
            for r in b.ranges() {
                for byte in r.start()..=r.end() {
                    if !byte.is_ascii() {
                        return None;
                    }
                    out.push(byte as char);
                    if out.len() > max_chars {
                        return None;
                    }
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `Spread`-adaptive multi-list Leapfrog sorted `EntityKey` intersection.
///
/// Uses exponential galloping seek when one list is much longer than the other,
/// running in `O(min_len * log(max_len / min_len))` instead of `O(sum_lens)`!
pub fn leapfrog_intersect(mut lists: Vec<Vec<EntityKey>>) -> Vec<EntityKey> {
    if lists.is_empty() {
        return Vec::new();
    }
    for l in &mut lists {
        l.sort_unstable();
        l.dedup();
        if l.is_empty() {
            return Vec::new();
        }
    }
    if lists.len() == 1 {
        return lists.pop().unwrap();
    }
    // Sort lists ascending by cardinality so the rarest list drives the leapfrog!
    lists.sort_by_key(Vec::len);

    let mut pos = vec![0usize; lists.len()];
    let mut out = Vec::with_capacity(lists[0].len());
    let mut max_key = lists[0][0];
    let mut p = 0usize;
    let k = lists.len();

    loop {
        let min_key = lists[p][pos[p]];
        if min_key == max_key {
            // Check if all lists are aligned on `max_key`!
            let mut all_eq = true;
            for i in 0..k {
                if lists[i][pos[i]] != max_key {
                    all_eq = false;
                    let target = max_key;
                    let next_idx = gallop_ge(&lists[i], pos[i], target);
                    if next_idx >= lists[i].len() {
                        return out;
                    }
                    pos[i] = next_idx;
                    if lists[i][next_idx] > max_key {
                        max_key = lists[i][next_idx];
                    }
                }
            }
            if all_eq {
                out.push(max_key);
                pos[0] += 1;
                if pos[0] >= lists[0].len() {
                    return out;
                }
                max_key = lists[0][pos[0]];
                p = 1 % k;
            }
        } else {
            let next_idx = gallop_ge(&lists[p], pos[p], max_key);
            if next_idx >= lists[p].len() {
                return out;
            }
            pos[p] = next_idx;
            if lists[p][next_idx] > max_key {
                max_key = lists[p][next_idx];
            }
            p = (p + 1) % k;
        }
    }
}

/// Exponential galloping search for the smallest index `>= start` with `slice[idx] >= target`.
#[inline]
fn gallop_ge(slice: &[EntityKey], start: usize, target: EntityKey) -> usize {
    if start >= slice.len() || slice[start] >= target {
        return start;
    }
    let mut step = 1usize;
    let mut lo = start;
    let mut hi = (start + step).min(slice.len());
    while hi < slice.len() && slice[hi] < target {
        lo = hi;
        step = step.saturating_mul(2);
        hi = start.saturating_add(step).min(slice.len());
    }
    lo + slice[lo..hi].partition_point(|&k| k < target)
}

/// Flattens top-level `FilterExpr::And` conjuncts into a flat list for pushdown analysis.
pub fn flatten_and(expr: &FilterExpr, out: &mut Vec<FilterExpr>) {
    match expr {
        FilterExpr::And(parts) => {
            for p in parts {
                flatten_and(p, out);
            }
        }
        other => out.push(other.clone()),
    }
}

/// Computes (or fetches from `PlanCache`) the high-level access strategy for `plan`.
pub fn optimize_plan<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    schema: &CompiledSchema,
    plan: &LogicalPlan,
    cache: Option<&PlanCache>,
) -> Result<(CachedStrategy, bool)> {
    let shape_key = PlanShapeKey::from_plan(schema.epoch, plan);
    if let Some(c) = cache
        && let Some(hit) = c.get(&shape_key)
    {
        // Verify any cached index is still Ready (e.g., after a rebuild/failure within same epoch).
        let valid = hit.chosen_index.is_none_or(|id| is_index_ready(txn, dbs, id).unwrap_or(false))
            && hit.use_index_order.is_none_or(|id| is_index_ready(txn, dbs, id).unwrap_or(false))
            && hit.use_trigram_index.is_none_or(|id| is_index_ready(txn, dbs, id).unwrap_or(false));
        if valid {
            return Ok((hit, true));
        }
    }

    let strategy = compute_strategy(txn, dbs, schema, &plan.root)?;
    if let Some(c) = cache {
        c.put(shape_key, strategy.clone());
    }
    Ok((strategy, false))
}

fn compute_strategy<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    schema: &CompiledSchema,
    root: &LogicalNode,
) -> Result<CachedStrategy> {
    // Detect `Terminal(Count)` over `Filter(Eq)` over `Source(Type { include_trashed: false })` -> O(1) count pushdown!
    if let LogicalNode::Terminal { input, terminal: TerminalSpec::Count } = root
        && let LogicalNode::Filter { input: src, expr: FilterExpr::Eq(field, val) } = input.as_ref()
        && !matches!(
            val,
            CanonicalValue::Null | CanonicalValue::Object(_) | CanonicalValue::Array(_) | CanonicalValue::Bytes(_)
        )
        && !matches!(val, CanonicalValue::String(s) if s.len() > elmdb_projections::MAX_INDEXED_STRING)
        && let LogicalNode::Source(SourceSpec::Type { type_id, include_trashed: false }) = src.as_ref()
        && let Some(idx) = find_ready_value_index(txn, dbs, schema, *type_id, *field)?
    {
        return Ok(CachedStrategy {
            access_label: format!("O1CountPushdown({})", idx.name),
            chosen_index: Some(idx.id),
            use_count_pushdown: true,
            use_index_order: None,
            use_trigram_index: None,
        });
    }

    // Walk down to find base `(TypeId, include_trashed)`, any Filter, and any OrderSlice!
    let mut base_type: Option<TypeId> = None;
    let mut chosen_index: Option<IndexId> = None;
    let mut use_index_order: Option<IndexId> = None;
    let mut use_trigram_index: Option<IndexId> = None;
    let mut access_label = "PrimaryScan".to_owned();

    // Find source type first.
    let mut cur = root;
    loop {
        match cur {
            LogicalNode::Source(SourceSpec::Type { type_id, include_trashed: false }) => {
                base_type = Some(*type_id);
                break;
            }
            LogicalNode::Source(_) => break,
            LogicalNode::Filter { input, .. }
            | LogicalNode::Score { input, .. }
            | LogicalNode::Traverse { input, .. }
            | LogicalNode::GraphAnalyze { input, .. }
            | LogicalNode::Distinct { input }
            | LogicalNode::OrderSlice { input, .. }
            | LogicalNode::Terminal { input, .. } => cur = input.as_ref(),
        }
    }

    if let Some(tid) = base_type {
        // Inspect OrderSlice and Filter nodes above the base type.
        let mut walk = root;
        loop {
            match walk {
                LogicalNode::OrderSlice { input, order: OrderSpec::Field { field, locale, .. }, .. } => {
                    if let Some(loc) = locale {
                        if let Some(idx) = find_ready_order_index(txn, dbs, schema, tid, *field, loc)? {
                            use_index_order = Some(idx.id);
                        }
                    } else if let Some(idx) = find_ready_value_index(txn, dbs, schema, tid, *field)? {
                        use_index_order = Some(idx.id);
                    }
                    walk = input.as_ref();
                }
                LogicalNode::Filter { input, expr } => {
                    let mut conjuncts = Vec::new();
                    flatten_and(expr, &mut conjuncts);
                    for c in &conjuncts {
                        match c {
                            FilterExpr::Eq(f, _)
                            | FilterExpr::In(f, _)
                            | FilterExpr::Range { field: f, .. }
                            | FilterExpr::StartsWith(f, _) => {
                                if chosen_index.is_none()
                                    && let Some(idx) = find_ready_value_index(txn, dbs, schema, tid, *f)?
                                {
                                    chosen_index = Some(idx.id);
                                    access_label = format!("ValueIndex({})", idx.name);
                                }
                            }
                            FilterExpr::Contains(f, _) | FilterExpr::Regex(f, _) => {
                                if use_trigram_index.is_none()
                                    && let Some(idx) = find_ready_trigram_index(txn, dbs, schema, tid, *f)?
                                {
                                    use_trigram_index = Some(idx.id);
                                    if chosen_index.is_none() {
                                        access_label = format!("TrigramIndex({})", idx.name);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    walk = input.as_ref();
                }
                LogicalNode::Source(_) => break,
                LogicalNode::Score { input, .. }
                | LogicalNode::Traverse { input, .. }
                | LogicalNode::GraphAnalyze { input, .. }
                | LogicalNode::Distinct { input }
                | LogicalNode::OrderSlice { input, .. }
                | LogicalNode::Terminal { input, .. } => walk = input.as_ref(),
            }
        }
    }

    Ok(CachedStrategy { access_label, chosen_index, use_count_pushdown: false, use_index_order, use_trigram_index })
}
