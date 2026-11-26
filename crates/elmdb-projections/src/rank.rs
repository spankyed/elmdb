//! Dynamic Zhang-Lofgren-Goel Push PageRank projection (1 sub-DB per rank index, 1-byte table tags).
//!
//! Maintains the exact linear residual invariant for every live entity `v` of `type_id`:
//!   `r(v) = (1 - α) + α * Σ_{u -> v, out_w(u) > 0} (w(u, v) / out_w(u)) * x(u) - x(v)`
//!
//! Online L1 error bound: `||x_exact - x||_1 <= (Σ_v |r(v)|) / (1 - α)`.
//!
//! Layout inside the index's pooled `SubDbSlot`:
//! - `0x00` -> `RankMeta { damping: f64, epsilon: f64, node_count: u64 }`
//! - `0x01 || EntityKey(8B BE)` -> `RankNode { x: f64, r: f64, out_weight: f64 }` (24 B LE)
//! - `0x02 || inv_sortable_f64(x)(8B BE) || EntityKey(8B BE)` -> `()` (descending score order!)
//! - `0x03 || EntityKey(8B BE)` -> `()` (dirty nodes with `|r(v)| > epsilon` queued for bounded push drain)

use std::collections::{BTreeMap, BTreeSet};

use elmdb_engine::{
    EdgeDelta, Engine, EntityDelta, EntityKey, IndexId, IndexKind, RelKindId, SystemDbs, TypeId, read_packed_row,
    scan_adj_in_raw, scan_adj_out_raw, scan_type_keys,
};
use elmdb_storage::{Clock, DbFlags, KvCursor, KvRead, KvStore, SubDbHandle, Vfs};
use elmdb_wire::{ElmdbError, ErrorCode, Result};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::state::{IndexMismatch, KvMut};

const TAG_META: u8 = 0x00;
const TAG_NODES: u8 = 0x01;
const TAG_ORDER: u8 = 0x02;
const TAG_DIRTY: u8 = 0x03;

pub const DEFAULT_DAMPING: f64 = 0.85;
pub const DEFAULT_EPSILON: f64 = 1e-6;
pub const DEFAULT_PUSH_BUDGET: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankMeta {
    pub damping: f64,
    pub epsilon: f64,
    pub node_count: u64,
}

impl Default for RankMeta {
    fn default() -> Self {
        Self { damping: DEFAULT_DAMPING, epsilon: DEFAULT_EPSILON, node_count: 0 }
    }
}

impl RankMeta {
    fn encode(self) -> [u8; 24] {
        let mut buf = [0u8; 24];
        buf[0..8].copy_from_slice(&self.damping.to_le_bytes());
        buf[8..16].copy_from_slice(&self.epsilon.to_le_bytes());
        buf[16..24].copy_from_slice(&self.node_count.to_le_bytes());
        buf
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() != 24 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt rank meta"));
        }
        let damping = f64::from_le_bytes(raw[0..8].try_into().expect("8"));
        let epsilon = f64::from_le_bytes(raw[8..16].try_into().expect("8"));
        let node_count = u64::from_le_bytes(raw[16..24].try_into().expect("8"));
        if !damping.is_finite() || !(0.0..1.0).contains(&damping) || !epsilon.is_finite() || epsilon <= 0.0 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "invalid rank meta parameters"));
        }
        Ok(Self { damping, epsilon, node_count })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankNode {
    pub x: f64,
    pub r: f64,
    pub out_weight: f64,
}

impl RankNode {
    fn encode(self) -> [u8; 24] {
        let mut buf = [0u8; 24];
        buf[0..8].copy_from_slice(&self.x.to_le_bytes());
        buf[8..16].copy_from_slice(&self.r.to_le_bytes());
        buf[16..24].copy_from_slice(&self.out_weight.to_le_bytes());
        buf
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() != 24 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt rank node"));
        }
        let x = f64::from_le_bytes(raw[0..8].try_into().expect("8"));
        let r = f64::from_le_bytes(raw[8..16].try_into().expect("8"));
        let out_weight = f64::from_le_bytes(raw[16..24].try_into().expect("8"));
        if !x.is_finite() || !r.is_finite() || !out_weight.is_finite() || out_weight < 0.0 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "non-finite rank node values"));
        }
        Ok(Self { x, r, out_weight })
    }
}

#[inline]
fn inv_sortable_f64(val: f64) -> [u8; 8] {
    let norm = if val == 0.0 { 0.0 } else { val };
    let bits = norm.to_bits();
    let ordered = if (bits >> 63) != 0 { !bits } else { bits ^ (1u64 << 63) };
    (!ordered).to_be_bytes()
}

fn node_key(key: EntityKey) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = TAG_NODES;
    k[1..9].copy_from_slice(&key.to_be_bytes());
    k
}

fn order_key(x: f64, key: EntityKey) -> [u8; 17] {
    let mut k = [0u8; 17];
    k[0] = TAG_ORDER;
    k[1..9].copy_from_slice(&inv_sortable_f64(x));
    k[9..17].copy_from_slice(&key.to_be_bytes());
    k
}

fn dirty_key(key: EntityKey) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = TAG_DIRTY;
    k[1..9].copy_from_slice(&key.to_be_bytes());
    k
}

#[inline]
fn effective_weight(raw: f64, weighted: bool) -> f64 {
    if !weighted {
        1.0
    } else if raw.is_finite() && raw > 0.0 {
        raw
    } else {
        0.0
    }
}

fn load_meta<R: KvRead>(txn: &R, db: SubDbHandle) -> Result<RankMeta> {
    match txn.get(db, &[TAG_META])? {
        Some(raw) => RankMeta::decode(raw),
        None => Ok(RankMeta::default()),
    }
}

fn load_node<R: KvRead>(txn: &R, db: SubDbHandle, key: EntityKey) -> Result<Option<RankNode>> {
    match txn.get(db, &node_key(key))? {
        Some(raw) => Ok(Some(RankNode::decode(raw)?)),
        None => Ok(None),
    }
}

fn save_node<W: KvMut>(
    txn: &mut W,
    db: SubDbHandle,
    key: EntityKey,
    old_x: Option<f64>,
    node: RankNode,
    epsilon: f64,
) -> Result<()> {
    if !node.x.is_finite() || !node.r.is_finite() || !node.out_weight.is_finite() || node.out_weight < 0.0 {
        return Err(ElmdbError::new(ErrorCode::Internal, "non-finite Push PageRank node state"));
    }
    if let Some(ox) = old_x
        && ox.to_bits() != node.x.to_bits()
    {
        txn.del(db, &order_key(ox, key))?;
        txn.put(db, &order_key(node.x, key), &[])?;
    } else if old_x.is_none() {
        txn.put(db, &order_key(node.x, key), &[])?;
    }
    txn.put(db, &node_key(key), &node.encode())?;
    if node.r.abs() > epsilon {
        txn.put(db, &dirty_key(key), &[])?;
    } else {
        txn.del(db, &dirty_key(key))?;
    }
    Ok(())
}

/// Computes overflow- and underflow-proof normalized transition probabilities `(dst, prob)` summing to `1.0`,
/// plus the normalized out-weight sum (`0.0` if no edges, else in `[1.0, edges.len()]`).
fn normalized_transitions(edges: &[(EntityKey, f64)]) -> (Vec<(EntityKey, f64)>, f64) {
    let max_w = edges.iter().map(|(_, w)| *w).fold(0.0f64, f64::max);
    if max_w <= 0.0 || !max_w.is_finite() {
        return (Vec::new(), 0.0);
    }
    let mut norm_sum = 0.0f64;
    for &(_, w) in edges {
        norm_sum += w / max_w;
    }
    if norm_sum <= 0.0 || !norm_sum.is_finite() {
        return (Vec::new(), 0.0);
    }
    let probs = edges.iter().map(|&(dst, w)| (dst, (w / max_w) / norm_sum)).collect();
    (probs, norm_sum)
}

fn live_out_edges<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    db: SubDbHandle,
    src: EntityKey,
    relations: &[RelKindId],
    weighted: bool,
) -> Result<Vec<(EntityKey, f64)>> {
    let raw = scan_adj_out_raw(txn, dbs, src, relations)?;
    let mut out = Vec::with_capacity(raw.len());
    for (dst, w, _) in raw {
        let ew = effective_weight(w, weighted);
        if ew > 0.0 && txn.get(db, &node_key(dst))?.is_some() {
            out.push((dst, ew));
        }
    }
    Ok(out)
}

/// Borrowed specification for a Push PageRank index.
#[derive(Debug, Clone, Copy)]
pub struct RankSpec<'a> {
    pub type_id: TypeId,
    pub relations: &'a [RelKindId],
    pub weighted: bool,
}

/// Applies coalesced entity & edge deltas to a Push PageRank index while preserving the exact ZLG invariant.
pub fn apply_deltas<W: KvMut>(
    txn: &mut W,
    dbs: &SystemDbs,
    db: SubDbHandle,
    spec: RankSpec<'_>,
    entity_deltas: &[EntityDelta],
    edge_deltas: &[EdgeDelta],
) -> Result<()> {
    let type_id = spec.type_id;
    let relations = spec.relations;
    let weighted = spec.weighted;
    let mut meta = load_meta(txn, db)?;
    let alpha = meta.damping;
    let teleport = 1.0 - alpha;
    let rel_set: FxHashSet<RelKindId> = relations.iter().copied().collect();

    // Track entity liveness transitions affecting `type_id`.
    let mut newly_live: Vec<EntityKey> = Vec::new();
    let mut newly_dead: Vec<EntityKey> = Vec::new();
    for d in entity_deltas {
        if d.type_id != type_id || !d.liveness_changed {
            continue;
        }
        let was_live = d.before.as_ref().is_some_and(|r| !r.header.is_trashed());
        let is_live = d.after.as_ref().is_some_and(|r| !r.header.is_trashed());
        if !was_live && is_live {
            newly_live.push(d.key);
        } else if was_live && !is_live {
            newly_dead.push(d.key);
        }
    }

    // Group edge deltas affecting tracked relation kinds by source vertex so multi-edge mutations on the
    // same vertex adjust residuals in one exact O(deg(u)) step!
    let mut edges_by_src: BTreeMap<EntityKey, Vec<&EdgeDelta>> = BTreeMap::new();
    let mut touched_sources: BTreeSet<EntityKey> = BTreeSet::new();
    for ed in edge_deltas {
        if !rel_set.is_empty() && !rel_set.contains(&ed.kind) {
            continue;
        }
        let before_src = ed.before.as_ref().map(|b| b.src);
        if let Some(src) = before_src {
            edges_by_src.entry(src).or_default().push(ed);
            touched_sources.insert(src);
        }
        if let Some(ref a) = ed.after
            && before_src != Some(a.src)
        {
            edges_by_src.entry(a.src).or_default().push(ed);
            touched_sources.insert(a.src);
        }
    }

    // Any live vertex with an edge pointing to a newly-dead or newly-live entity also changes its active out_weight!
    for &k in newly_dead.iter().chain(newly_live.iter()) {
        for (src, _, _) in scan_adj_in_raw(txn, dbs, k, relations)? {
            touched_sources.insert(src);
        }
        touched_sources.insert(k);
    }

    if newly_live.is_empty() && newly_dead.is_empty() && touched_sources.is_empty() {
        return Ok(());
    }

    // Snapshot pre-delta live node membership and outgoing transitions for every touched source BEFORE mutating `TAG_NODES`.
    let mut old_state: FxHashMap<EntityKey, (RankNode, Vec<(EntityKey, f64)>)> = FxHashMap::default();
    let dead_set: FxHashSet<EntityKey> = newly_dead.iter().copied().collect();
    let live_set: FxHashSet<EntityKey> = newly_live.iter().copied().collect();

    for &src in &touched_sources {
        let Some(node) = load_node(txn, db, src)? else {
            continue;
        };
        // Reconstruct `old_edges` from current `adj_out` minus `after` deltas plus `before` deltas,
        // filtering targets by pre-delta live membership (nodes currently in `TAG_NODES`, which has not yet
        // removed `newly_dead` or added `newly_live`!).
        let cur_raw = scan_adj_out_raw(txn, dbs, src, relations)?;
        let mut counts: BTreeMap<(EntityKey, u64), i64> = BTreeMap::new();
        for (dst, w, _) in cur_raw {
            let ew = effective_weight(w, weighted);
            if ew > 0.0 {
                *counts.entry((dst, ew.to_bits())).or_insert(0) += 1;
            }
        }
        if let Some(eds) = edges_by_src.get(&src) {
            for ed in eds {
                if let Some(ref a) = ed.after
                    && a.src == src
                {
                    let ew = effective_weight(a.weight, weighted);
                    if ew > 0.0 {
                        *counts.entry((a.dst, ew.to_bits())).or_insert(0) -= 1;
                    }
                }
                if let Some(ref b) = ed.before
                    && b.src == src
                {
                    let ew = effective_weight(b.weight, weighted);
                    if ew > 0.0 {
                        *counts.entry((b.dst, ew.to_bits())).or_insert(0) += 1;
                    }
                }
            }
        }
        let mut old_edges = Vec::new();
        for ((dst, w_bits), cnt) in counts {
            if cnt > 0 && txn.get(db, &node_key(dst))?.is_some() {
                let w = f64::from_bits(w_bits);
                for _ in 0..cnt {
                    old_edges.push((dst, w));
                }
            }
        }
        old_state.insert(src, (node, old_edges));
    }

    // Step 1: Add newly-live nodes (initially x = 0, r = teleport) and remove newly-dead nodes.
    for &k in &newly_live {
        if load_node(txn, db, k)?.is_none() {
            let n = RankNode { x: 0.0, r: teleport, out_weight: 0.0 };
            save_node(txn, db, k, None, n, meta.epsilon)?;
            meta.node_count = meta.node_count.saturating_add(1);
        }
    }
    for &k in &newly_dead {
        if let Some(old_node) = load_node(txn, db, k)? {
            // Subtract outgoing mass `k` was contributing to surviving neighbors!
            if old_node.x != 0.0
                && let Some((_, old_edges)) = old_state.get(&k)
            {
                let (old_probs, _) = normalized_transitions(old_edges);
                let mass = alpha * old_node.x;
                for (dst, prob) in old_probs {
                    if dst == k || dead_set.contains(&dst) {
                        continue;
                    }
                    if let Some(mut dn) = load_node(txn, db, dst)? {
                        dn.r -= mass * prob;
                        save_node(txn, db, dst, Some(dn.x), dn, meta.epsilon)?;
                    }
                }
            }
            txn.del(db, &node_key(k))?;
            txn.del(db, &order_key(old_node.x, k))?;
            txn.del(db, &dirty_key(k))?;
            meta.node_count = meta.node_count.saturating_sub(1);
        }
    }

    // Step 2: Reconcile outgoing transition deltas for every surviving touched source!
    let mut residual_adjust: BTreeMap<EntityKey, f64> = BTreeMap::new();
    for &src in &touched_sources {
        if dead_set.contains(&src) {
            continue;
        }
        let Some(mut node) = load_node(txn, db, src)? else {
            continue;
        };
        let new_edges = live_out_edges(txn, dbs, db, src, relations, weighted)?;
        let (new_probs, new_out_w) = normalized_transitions(&new_edges);

        // Only nodes that existed pre-delta with non-zero x could have pushed mass along old/new edges!
        if !live_set.contains(&src)
            && node.x != 0.0
            && let Some((_, old_edges)) = old_state.get(&src)
        {
            let (old_probs, _) = normalized_transitions(old_edges);
            let mass = alpha * node.x;
            for (dst, prob) in old_probs {
                if !dead_set.contains(&dst) {
                    *residual_adjust.entry(dst).or_insert(0.0) -= mass * prob;
                }
            }
            for (dst, prob) in &new_probs {
                *residual_adjust.entry(*dst).or_insert(0.0) += mass * prob;
            }
        }

        node.out_weight = new_out_w;
        save_node(txn, db, src, Some(node.x), node, meta.epsilon)?;
    }

    for (dst, delta_r) in residual_adjust {
        if delta_r == 0.0 {
            continue;
        }
        if let Some(mut dn) = load_node(txn, db, dst)? {
            dn.r += delta_r;
            save_node(txn, db, dst, Some(dn.x), dn, meta.epsilon)?;
        }
    }

    txn.put(db, &[TAG_META], &meta.encode())?;
    Ok(())
}

/// Runs up to `max_pushes` Zhang-Lofgren-Goel residual pushes on dirty nodes inside the given write txn.
/// Returns the number of pushes executed (`0` means fully converged within `epsilon`).
pub fn drain_pushes_in_txn<W: KvMut>(
    txn: &mut W,
    dbs: &SystemDbs,
    db: SubDbHandle,
    relations: &[RelKindId],
    weighted: bool,
    max_pushes: usize,
) -> Result<usize> {
    if max_pushes == 0 {
        return Ok(0);
    }
    let meta = load_meta(txn, db)?;
    let alpha = meta.damping;
    let eps = meta.epsilon;
    let mut pushes = 0usize;

    while pushes < max_pushes {
        // Pop a batch of dirty keys (up to 64 at a time to amortize cursor setup).
        let mut dirty_batch = Vec::new();
        {
            let mut cur = txn.cursor(db)?;
            let mut step = cur.seek_ge(&[TAG_DIRTY])?;
            while let Some((k, _)) = step {
                if k.first() != Some(&TAG_DIRTY) {
                    break;
                }
                if k.len() != 9 {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt dirty rank key"));
                }
                dirty_batch.push(EntityKey::from_be_bytes(k[1..9].try_into().expect("8")));
                if dirty_batch.len() >= 64.min(max_pushes - pushes) {
                    break;
                }
                step = cur.next()?;
            }
        }
        if dirty_batch.is_empty() {
            break;
        }

        for u in dirty_batch {
            let Some(mut unode) = load_node(txn, db, u)? else {
                txn.del(db, &dirty_key(u))?;
                continue;
            };
            if unode.r.abs() <= eps {
                txn.del(db, &dirty_key(u))?;
                continue;
            }
            let delta = unode.r;
            let old_x = unode.x;
            unode.x += delta;
            unode.r = 0.0;
            save_node(txn, db, u, Some(old_x), unode, eps)?;
            pushes += 1;

            if unode.out_weight > 0.0 {
                let edges = live_out_edges(txn, dbs, db, u, relations, weighted)?;
                let (probs, _) = normalized_transitions(&edges);
                let mass = alpha * delta;
                for (v, prob) in probs {
                    if let Some(mut vnode) = load_node(txn, db, v)? {
                        vnode.r += mass * prob;
                        save_node(txn, db, v, Some(vnode.x), vnode, eps)?;
                    }
                }
            }
            if pushes >= max_pushes {
                break;
            }
        }
    }
    Ok(pushes)
}

/// Drives bounded push PageRank drain commits on `engine` for `index_id` until `max_pushes` or convergence.
pub fn drain_rank_chunk<S: KvStore, V: Vfs, C: Clock>(
    engine: &Engine<S, V, C>,
    index_id: IndexId,
    max_pushes: usize,
) -> Result<usize> {
    // Fast RO pre-check so converged rank indexes never take the write lock!
    {
        let (ro, schema) = engine.begin_read_pinned()?;
        let def = schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        if !matches!(def.kind, IndexKind::Rank { .. }) {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "index is not a Rank index"));
        }
        if let Some(db) = engine.gate().open_subdb_ro(&ro, &def.subdb_slot().name(), DbFlags::DEFAULT)? {
            let mut cur = ro.cursor(db)?;
            let has_dirty = cur.seek_ge(&[TAG_DIRTY])?.is_some_and(|(k, _)| k.first() == Some(&TAG_DIRTY));
            if !has_dirty {
                return Ok(0);
            }
        } else {
            return Ok(0);
        }
    }

    let (pushes, _) = engine.run_maintenance_write(|txn, schema, dbs| {
        let def = schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        let IndexKind::Rank { relations, weighted } = &def.kind else {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "index is not a Rank index"));
        };
        let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
        drain_pushes_in_txn(txn, dbs, db, relations, *weighted, max_pushes)
    })?;
    Ok(pushes)
}

/// Drains a Push PageRank index to full convergence (`|r(v)| <= epsilon` for all nodes).
pub fn flush_rank<S: KvStore, V: Vfs, C: Clock>(engine: &Engine<S, V, C>, index_id: IndexId) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let n = drain_rank_chunk(engine, index_id, DEFAULT_PUSH_BUDGET)?;
        total += n;
        if n == 0 {
            break;
        }
    }
    Ok(total)
}

/// Initializes a Push PageRank index from primary entities + adjacency (exact ZLG invariant holds immediately!).
/// When `converge_now == true`, also runs ZLG pushes to convergence synchronously.
pub fn rebuild_in_txn<W: KvMut>(
    txn: &mut W,
    dbs: &SystemDbs,
    db: SubDbHandle,
    type_id: TypeId,
    relations: &[RelKindId],
    weighted: bool,
    converge_now: bool,
) -> Result<()> {
    txn.truncate(db)?;
    let mut meta = RankMeta::default();
    let teleport = 1.0 - meta.damping;

    let keys = scan_type_keys(txn, dbs, type_id)?;
    let mut live_keys = Vec::new();
    for ek in keys {
        if let Some(row) = read_packed_row(txn, dbs, ek)?
            && !row.header.is_trashed()
        {
            live_keys.push(ek);
        }
    }

    // Pass 1: Insert all live nodes so `live_out_edges` sees full membership.
    for &ek in &live_keys {
        let n = RankNode { x: 0.0, r: teleport, out_weight: 0.0 };
        save_node(txn, db, ek, None, n, meta.epsilon)?;
    }
    meta.node_count = live_keys.len() as u64;
    txn.put(db, &[TAG_META], &meta.encode())?;

    // Pass 2: Populate exact normalized `out_weight` for each live node.
    for &ek in &live_keys {
        let edges = live_out_edges(txn, dbs, db, ek, relations, weighted)?;
        let (_, ow) = normalized_transitions(&edges);
        if ow > 0.0 {
            let n = RankNode { x: 0.0, r: teleport, out_weight: ow };
            save_node(txn, db, ek, Some(0.0), n, meta.epsilon)?;
        }
    }

    if converge_now {
        let max_total = live_keys.len().max(1).saturating_mul(500).min(1_000_000);
        let _ = drain_pushes_in_txn(txn, dbs, db, relations, weighted, max_total)?;
    }
    Ok(())
}

/// Computes the exact online L1 error bound: `(Σ_v |r(v)|) / (1 - α)`.
pub fn l1_error_bound<R: KvRead>(txn: &R, db: SubDbHandle) -> Result<f64> {
    let meta = load_meta(txn, db)?;
    let mut sum_abs_r = 0.0f64;
    let mut cur = txn.cursor(db)?;
    let mut step = cur.seek_ge(&[TAG_NODES])?;
    while let Some((k, v)) = step {
        if k.first() != Some(&TAG_NODES) {
            break;
        }
        let node = RankNode::decode(v)?;
        sum_abs_r += node.r.abs();
        step = cur.next()?;
    }
    Ok(sum_abs_r / (1.0 - meta.damping))
}

/// Streams top-`k` entities directly from `TAG_ORDER` in O(k) (highest PageRank first unless `reverse == true`).
pub fn scan_ranked<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    reverse: bool,
    limit: Option<usize>,
) -> Result<Vec<(EntityKey, f64)>> {
    let cap = limit.unwrap_or(usize::MAX);
    if cap == 0 {
        return Ok(Vec::new());
    }
    let mut cur = txn.cursor(db)?;
    let mut out = Vec::new();
    let mut step = if !reverse {
        cur.seek_ge(&[TAG_ORDER])?
    } else {
        match cur.seek_ge(&[TAG_ORDER + 1])? {
            Some(_) => cur.prev()?,
            None => cur.last()?,
        }
    };
    while let Some((k, _)) = step {
        if k.first() != Some(&TAG_ORDER) {
            break;
        }
        if k.len() != 17 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt rank order key"));
        }
        let ek = EntityKey::from_be_bytes(k[9..17].try_into().expect("8"));
        let x = load_node(txn, db, ek)?
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, format!("rank order points to missing node {ek:?}")))?
            .x;
        out.push((ek, x));
        if out.len() >= cap {
            break;
        }
        step = if reverse { cur.prev()? } else { cur.next()? };
    }
    Ok(out)
}

/// Returns a single entity's current PageRank score `x` (or `None` if not in the rank index).
pub fn rank_of<R: KvRead>(txn: &R, db: SubDbHandle, key: EntityKey) -> Result<Option<f64>> {
    Ok(load_node(txn, db, key)?.map(|n| n.x))
}

/// Verifies every invariant of a Push PageRank index:
/// 1. Live node set in `TAG_NODES` == live entities of `type_id`
/// 2. `TAG_ORDER` matches `TAG_NODES` 1-to-1
/// 3. Exact ZLG linear residual invariant holds to `< 1e-8` for every node!
pub fn check_rank<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    db: Option<SubDbHandle>,
    index_id: IndexId,
    index_name: &str,
    spec: RankSpec<'_>,
) -> Result<Vec<IndexMismatch>> {
    let type_id = spec.type_id;
    let relations = spec.relations;
    let weighted = spec.weighted;
    let mut mismatches = Vec::new();
    let Some(db) = db else {
        for ek in scan_type_keys(txn, dbs, type_id)? {
            if let Some(row) = read_packed_row(txn, dbs, ek)?
                && !row.header.is_trashed()
            {
                mismatches.push(IndexMismatch {
                    index_id,
                    index_name: index_name.to_owned(),
                    detail: "rank sub-DB missing".into(),
                });
                break;
            }
        }
        return Ok(mismatches);
    };

    let meta = load_meta(txn, db)?;
    let alpha = meta.damping;
    let teleport = 1.0 - alpha;

    // 1. Collect actual nodes from TAG_NODES and compare with primary live entities.
    let mut nodes: BTreeMap<EntityKey, RankNode> = BTreeMap::new();
    {
        let mut cur = txn.cursor(db)?;
        let mut step = cur.seek_ge(&[TAG_NODES])?;
        while let Some((k, v)) = step {
            if k.first() != Some(&TAG_NODES) {
                break;
            }
            if k.len() != 9 {
                mismatches.push(IndexMismatch {
                    index_id,
                    index_name: index_name.to_owned(),
                    detail: "corrupt rank node key".into(),
                });
                return Ok(mismatches);
            }
            let ek = EntityKey::from_be_bytes(k[1..9].try_into().expect("8"));
            nodes.insert(ek, RankNode::decode(v)?);
            step = cur.next()?;
        }
    }

    let mut primary_live = BTreeSet::new();
    for ek in scan_type_keys(txn, dbs, type_id)? {
        if let Some(row) = read_packed_row(txn, dbs, ek)?
            && !row.header.is_trashed()
        {
            primary_live.insert(ek);
        }
    }
    if nodes.len() != primary_live.len() || nodes.keys().any(|k| !primary_live.contains(k)) {
        mismatches.push(IndexMismatch {
            index_id,
            index_name: index_name.to_owned(),
            detail: format!("rank node set mismatch: {} stored vs {} primary live", nodes.len(), primary_live.len()),
        });
        return Ok(mismatches);
    }

    // 2. Verify TAG_ORDER matches TAG_NODES 1-to-1.
    let mut order_count = 0usize;
    {
        let mut cur = txn.cursor(db)?;
        let mut step = cur.seek_ge(&[TAG_ORDER])?;
        while let Some((k, _)) = step {
            if k.first() != Some(&TAG_ORDER) {
                break;
            }
            if k.len() != 17 {
                mismatches.push(IndexMismatch {
                    index_id,
                    index_name: index_name.to_owned(),
                    detail: "corrupt rank order key".into(),
                });
                return Ok(mismatches);
            }
            let ek = EntityKey::from_be_bytes(k[9..17].try_into().expect("8"));
            let Some(n) = nodes.get(&ek) else {
                mismatches.push(IndexMismatch {
                    index_id,
                    index_name: index_name.to_owned(),
                    detail: format!("stale rank order entry for {ek:?}"),
                });
                return Ok(mismatches);
            };
            if k[1..9] != inv_sortable_f64(n.x) {
                mismatches.push(IndexMismatch {
                    index_id,
                    index_name: index_name.to_owned(),
                    detail: format!("rank order score key mismatch for {ek:?}"),
                });
                return Ok(mismatches);
            }
            order_count += 1;
            step = cur.next()?;
        }
    }
    if order_count != nodes.len() {
        mismatches.push(IndexMismatch {
            index_id,
            index_name: index_name.to_owned(),
            detail: format!("rank order count {} != node count {}", order_count, nodes.len()),
        });
    }

    if meta.node_count != nodes.len() as u64 {
        mismatches.push(IndexMismatch {
            index_id,
            index_name: index_name.to_owned(),
            detail: format!("rank meta.node_count {} != actual nodes {}", meta.node_count, nodes.len()),
        });
    }

    // 3. Recompute exact normalized `out_weight`, `TAG_DIRTY` coverage, and exact ZLG residual!
    let mut incoming_mass: FxHashMap<EntityKey, f64> = FxHashMap::default();
    for (&u, unode) in &nodes {
        let edges = live_out_edges(txn, dbs, db, u, relations, weighted)?;
        let (probs, ow) = normalized_transitions(&edges);
        if (ow - unode.out_weight).abs() > 1e-9 * ow.max(1.0) {
            mismatches.push(IndexMismatch {
                index_id,
                index_name: index_name.to_owned(),
                detail: format!("out_weight mismatch on {u:?}: stored={} expected={ow}", unode.out_weight),
            });
            if mismatches.len() >= 16 {
                return Ok(mismatches);
            }
        }
        if unode.r.abs() > meta.epsilon && txn.get(db, &dirty_key(u))?.is_none() {
            mismatches.push(IndexMismatch {
                index_id,
                index_name: index_name.to_owned(),
                detail: format!("dirty node {u:?} (|r|={}) missing from TAG_DIRTY", unode.r.abs()),
            });
            if mismatches.len() >= 16 {
                return Ok(mismatches);
            }
        }
        if ow > 0.0 && unode.x != 0.0 {
            let mass = alpha * unode.x;
            for (v, prob) in probs {
                *incoming_mass.entry(v).or_insert(0.0) += mass * prob;
            }
        }
    }

    for (&v, vnode) in &nodes {
        let expected_r = teleport + incoming_mass.get(&v).copied().unwrap_or(0.0) - vnode.x;
        let tol = 1e-8 * vnode.x.abs().max(1.0);
        if (vnode.r - expected_r).abs() > tol {
            mismatches.push(IndexMismatch {
                index_id,
                index_name: index_name.to_owned(),
                detail: format!("ZLG residual invariant violated on {v:?}: stored={} expected={expected_r}", vnode.r),
            });
            if mismatches.len() >= 16 {
                return Ok(mismatches);
            }
        }
    }
    Ok(mismatches)
}
