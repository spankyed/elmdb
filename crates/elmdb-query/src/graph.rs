//! Deterministic graph analytics suite (zero-alloc `EntityKey(u64)` CSR projections)
//! and Shewchuk's exact order-independent floating-point expansion `ExactSum`.
//!
//! Preserves all 7 v1 graph algorithms:
//! `Ppr`, `WeakComponents`, `Scc` (iterative Tarjan), `TopoSort` (Kahn over condensation),
//! `Leiden` (`leiden-rs 0.8.1`), `Jaccard`, and `AdamicAdar` (`libm::log`).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};

use elmdb_engine::{EntityKey, RelKindId, SystemDbs, row::decode_row_header, scan_adj_in_raw, scan_adj_out_raw};
use elmdb_storage::KvRead;
use elmdb_wire::{ElmdbError, ErrorCode, Result};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::EdgeDir;

#[inline]
fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if let Some(flag) = cancel
        && flag.load(AtomicOrd::Relaxed)
    {
        return Err(ElmdbError::new(ErrorCode::Cancelled, "query cancelled"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shewchuk's Exact Floating-Point Expansion (`ExactSum`)
// ---------------------------------------------------------------------------

/// Order-independent exact floating-point accumulator (Shewchuk expansion), rounding once to nearest-even.
#[derive(Debug, Default, Clone)]
pub struct ExactSum {
    parts: Vec<f64>,
}

impl ExactSum {
    /// Adds `x` to the expansion. Returns `false` (leaving the accumulator unchanged) if `x` is
    /// non-finite or adding `x` would overflow `f64`.
    pub fn add(&mut self, mut x: f64) -> bool {
        if !x.is_finite() {
            return false;
        }
        if x == 0.0 {
            return true;
        }
        let mut next = Vec::with_capacity(self.parts.len() + 1);
        for &part in &self.parts {
            let mut y = part;
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let sum = x + y;
            if !sum.is_finite() {
                return false;
            }
            let lost = y - (sum - x);
            if lost != 0.0 {
                next.push(lost);
            }
            x = sum;
        }
        if !x.is_finite() {
            return false;
        }
        if x != 0.0 {
            next.push(x);
        }
        self.parts = next;
        true
    }

    pub fn total(&self) -> f64 {
        let mut parts = self.parts.iter().rev();
        let mut total = parts.next().copied().unwrap_or(0.0);
        let mut lost = 0.0;
        for &part in parts.by_ref() {
            let larger = total;
            total = larger + part;
            lost = part - (total - larger);
            if lost != 0.0 {
                break;
            }
        }
        if let Some(&below) = parts.next()
            && ((lost < 0.0 && below < 0.0) || (lost > 0.0 && below > 0.0))
        {
            let twice = lost * 2.0;
            let other = total + twice;
            if twice == other - total {
                total = other;
            }
        }
        total + 0.0
    }
}

// ---------------------------------------------------------------------------
// Zero-Alloc `EntityKey(u64)` CSR Graph Projection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CsrGraph {
    pub keys: Vec<EntityKey>,
    pub key_to_idx: FxHashMap<EntityKey, u32>,
    pub offsets: Vec<u32>,
    pub targets: Vec<u32>,
    pub weights: Option<Vec<f64>>,
}

/// Budgets and read-tracking callbacks for CSR builds and neighborhood similarity queries.
pub struct GraphBuildOpts<'a> {
    pub max_heap_bytes: usize,
    pub cancel: Option<&'a AtomicBool>,
    pub include_trashed: bool,
}

impl Default for GraphBuildOpts<'_> {
    fn default() -> Self {
        Self { max_heap_bytes: 16 * 1024 * 1024, cancel: None, include_trashed: false }
    }
}

impl CsrGraph {
    /// Builds a deterministic CSR subgraph induced by `nodes` (sorted ascending by `EntityKey`)
    /// directly from `adj_out` with zero string allocations, respecting `max_heap_bytes` and `cancel`.
    pub fn build<R: KvRead>(
        txn: &R,
        dbs: &SystemDbs,
        nodes: Vec<EntityKey>,
        rel_kinds: &[RelKindId],
        weighted: bool,
        max_heap_bytes: usize,
    ) -> Result<Self> {
        Self::build_tracked(
            txn,
            dbs,
            nodes,
            rel_kinds,
            weighted,
            &GraphBuildOpts { max_heap_bytes, cancel: None, include_trashed: true },
            &mut |_| {},
        )
    }

    /// Tracked CSR subgraph builder invoking `on_adj_read(u)` for every node whose adjacency list is scanned.
    pub fn build_tracked<R: KvRead>(
        txn: &R,
        dbs: &SystemDbs,
        mut nodes: Vec<EntityKey>,
        rel_kinds: &[RelKindId],
        weighted: bool,
        opts: &GraphBuildOpts<'_>,
        on_adj_read: &mut dyn FnMut(EntityKey),
    ) -> Result<Self> {
        check_cancel(opts.cancel)?;
        nodes.sort_unstable();
        nodes.dedup();
        let n = nodes.len();
        let mut approx_bytes = n.saturating_mul(32);
        if approx_bytes > opts.max_heap_bytes {
            return Err(ElmdbError::new(ErrorCode::ResourceLimit, "graph projection exceeds memory budget"));
        }
        let mut key_to_idx = FxHashMap::default();
        key_to_idx.reserve(n);
        for (i, &k) in nodes.iter().enumerate() {
            key_to_idx.insert(k, i as u32);
        }

        let mut offsets = Vec::with_capacity(n + 1);
        offsets.push(0u32);
        let mut targets = Vec::new();
        let mut weights = weighted.then(Vec::new);

        for (step_idx, &u_key) in nodes.iter().enumerate() {
            if step_idx & 63 == 0 {
                check_cancel(opts.cancel)?;
            }
            on_adj_read(u_key);
            let mut edges = scan_adj_out_raw(txn, dbs, u_key, rel_kinds)?;
            // Deterministic edge ordering by target EntityKey ascending.
            edges.sort_by_key(|(dst, _, _)| *dst);
            for (dst, w, _) in edges {
                if let Some(&v_idx) = key_to_idx.get(&dst) {
                    let ew = if weighted { if w.is_finite() && w > 0.0 { w } else { 0.0 } } else { 1.0 };
                    if weighted && ew <= 0.0 {
                        continue;
                    }
                    targets.push(v_idx);
                    if let Some(ref mut ws) = weights {
                        ws.push(ew);
                    }
                    approx_bytes = approx_bytes.saturating_add(if weighted { 12 } else { 4 });
                    if approx_bytes > opts.max_heap_bytes {
                        return Err(ElmdbError::new(
                            ErrorCode::ResourceLimit,
                            "graph projection edges exceed memory budget",
                        ));
                    }
                }
            }
            offsets.push(
                u32::try_from(targets.len())
                    .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "graph edge count exceeds u32"))?,
            );
        }

        Ok(Self { keys: nodes, key_to_idx, offsets, targets, weights })
    }

    pub fn node_count(&self) -> usize {
        self.keys.len()
    }

    pub fn out(&self, v: usize) -> &[u32] {
        &self.targets[self.offsets[v] as usize..self.offsets[v + 1] as usize]
    }

    pub fn out_weights(&self, v: usize) -> Option<&[f64]> {
        self.weights.as_ref().map(|ws| &ws[self.offsets[v] as usize..self.offsets[v + 1] as usize])
    }
}

/// Deterministic canonical community / component grouping:
/// groups ordered largest first, ties by lowest `EntityKey`; members sorted ascending.
pub fn canonical_groups(keys: &[EntityKey], labels: &[u32]) -> Vec<Vec<EntityKey>> {
    let mut by_label: FxHashMap<u32, Vec<EntityKey>> = FxHashMap::default();
    for (&k, &lbl) in keys.iter().zip(labels.iter()) {
        by_label.entry(lbl).or_default().push(k);
    }
    let mut groups: Vec<Vec<EntityKey>> = by_label
        .into_values()
        .map(|mut members| {
            members.sort_unstable();
            members
        })
        .collect();
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
    groups
}

/// #1 Weakly Connected Components (undirected BFS over CSR).
pub fn weak_components(graph: &CsrGraph) -> Vec<Vec<EntityKey>> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }
    let mut undir: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (u, u_adj) in undir.iter_mut().enumerate() {
        u_adj.extend_from_slice(graph.out(u));
    }
    for u in 0..n {
        for &v in graph.out(u) {
            undir[v as usize].push(u as u32);
        }
    }
    let mut labels = vec![u32::MAX; n];
    let mut next_lbl = 0u32;
    let mut q = VecDeque::new();
    for start in 0..n {
        if labels[start] != u32::MAX {
            continue;
        }
        labels[start] = next_lbl;
        q.push_back(start as u32);
        while let Some(u) = q.pop_front() {
            for &v in &undir[u as usize] {
                if labels[v as usize] == u32::MAX {
                    labels[v as usize] = next_lbl;
                    q.push_back(v);
                }
            }
        }
        next_lbl += 1;
    }
    canonical_groups(&graph.keys, &labels)
}

/// #2 Strongly Connected Components (iterative Tarjan — zero recursion stack overflow!).
pub fn scc(graph: &CsrGraph) -> Vec<Vec<EntityKey>> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }
    let mut index = vec![u32::MAX; n];
    let mut lowlink = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut tarjan_stack: Vec<u32> = Vec::new();
    let mut call_stack: Vec<(u32, usize)> = Vec::new();
    let mut labels = vec![0u32; n];
    let mut cur_index = 0u32;
    let mut cur_comp = 0u32;

    for start in 0..n {
        if index[start] != u32::MAX {
            continue;
        }
        index[start] = cur_index;
        lowlink[start] = cur_index;
        cur_index += 1;
        tarjan_stack.push(start as u32);
        on_stack[start] = true;
        call_stack.push((start as u32, 0));

        while let Some((v, next_i)) = call_stack.pop() {
            let neighs = graph.out(v as usize);
            if next_i < neighs.len() {
                call_stack.push((v, next_i + 1));
                let w = neighs[next_i];
                if index[w as usize] == u32::MAX {
                    index[w as usize] = cur_index;
                    lowlink[w as usize] = cur_index;
                    cur_index += 1;
                    tarjan_stack.push(w);
                    on_stack[w as usize] = true;
                    call_stack.push((w, 0));
                } else if on_stack[w as usize] {
                    lowlink[v as usize] = lowlink[v as usize].min(index[w as usize]);
                }
            } else {
                if let Some(&(parent, _)) = call_stack.last() {
                    lowlink[parent as usize] = lowlink[parent as usize].min(lowlink[v as usize]);
                }
                if lowlink[v as usize] == index[v as usize] {
                    while let Some(w) = tarjan_stack.pop() {
                        on_stack[w as usize] = false;
                        labels[w as usize] = cur_comp;
                        if w == v {
                            break;
                        }
                    }
                    cur_comp += 1;
                }
            }
        }
    }
    canonical_groups(&graph.keys, &labels)
}

/// #3 Topological sort over SCC condensation (Kahn's algorithm with deterministic tie-breaking + cycle count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopoResult {
    pub order: Vec<EntityKey>,
    pub cycles: u32,
}

pub fn topo_sort(graph: &CsrGraph) -> TopoResult {
    let n = graph.node_count();
    if n == 0 {
        return TopoResult { order: Vec::new(), cycles: 0 };
    }
    let comps = scc(graph);
    let mut comp_of = vec![0u32; n];
    for (ci, members) in comps.iter().enumerate() {
        for &k in members {
            let idx = graph.key_to_idx[&k] as usize;
            comp_of[idx] = ci as u32;
        }
    }
    let mut indeg = vec![0u32; comps.len()];
    let mut comp_edges: Vec<Vec<u32>> = vec![Vec::new(); comps.len()];
    let mut cycles = 0u32;

    for (ci, members) in comps.iter().enumerate() {
        let mut has_cycle = members.len() > 1;
        for &k in members {
            let u = graph.key_to_idx[&k] as usize;
            for &v in graph.out(u) {
                let to = comp_of[v as usize];
                if to as usize == ci {
                    has_cycle = true;
                } else {
                    comp_edges[ci].push(to);
                    indeg[to as usize] += 1;
                }
            }
        }
        cycles += u32::from(has_cycle);
    }

    let mut ready: BinaryHeap<Reverse<(EntityKey, u32)>> = BinaryHeap::new();
    for (ci, &deg) in indeg.iter().enumerate() {
        if deg == 0 {
            ready.push(Reverse((comps[ci][0], ci as u32)));
        }
    }
    let mut order = Vec::with_capacity(n);
    while let Some(Reverse((_, ci))) = ready.pop() {
        order.extend_from_slice(&comps[ci as usize]);
        for &to in &comp_edges[ci as usize] {
            indeg[to as usize] -= 1;
            if indeg[to as usize] == 0 {
                ready.push(Reverse((comps[to as usize][0], to)));
            }
        }
    }
    TopoResult { order, cycles }
}

/// #4 Personalized PageRank by deterministic power iteration (scores sum to 1.0).
#[derive(Debug, Clone, PartialEq)]
pub struct PprResult {
    pub scores: Vec<(EntityKey, f64)>,
    pub converged: bool,
    pub iterations: u32,
}

pub fn ppr(
    graph: &CsrGraph,
    seeds: &[(EntityKey, f64)],
    damping: f64,
    iterations: u32,
    tolerance: f64,
) -> Result<PprResult> {
    ppr_cancellable(graph, seeds, damping, iterations, tolerance, None)
}

pub fn ppr_cancellable(
    graph: &CsrGraph,
    seeds: &[(EntityKey, f64)],
    damping: f64,
    iterations: u32,
    tolerance: f64,
    cancel: Option<&AtomicBool>,
) -> Result<PprResult> {
    if !damping.is_finite() || !(0.0..1.0).contains(&damping) {
        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "PPR damping must be in [0, 1)"));
    }
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "PPR tolerance must be finite and >= 0"));
    }
    for &(_, w) in seeds {
        if !w.is_finite() || w < 0.0 {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "PPR seed weight must be finite and >= 0"));
        }
    }
    let n = graph.node_count();
    if n == 0 {
        return Ok(PprResult { scores: Vec::new(), converged: true, iterations: 0 });
    }
    let mut teleport = vec![0.0f64; n];
    if seeds.is_empty() {
        let uniform = 1.0 / (n as f64);
        teleport.fill(uniform);
    } else {
        // Max-weight normalization prevents `seed_sum` overflow (e.g. `[1e308, 1e308]`)!
        let mut max_seed_w = 0.0f64;
        for &(sk, w) in seeds {
            if w > 0.0 && graph.key_to_idx.contains_key(&sk) {
                max_seed_w = max_seed_w.max(w);
            }
        }
        if max_seed_w <= 0.0 {
            return Ok(PprResult { scores: Vec::new(), converged: true, iterations: 0 });
        }
        let mut seed_sum = 0.0f64;
        for &(sk, w) in seeds {
            if w > 0.0
                && let Some(&idx) = graph.key_to_idx.get(&sk)
            {
                let nw = w / max_seed_w;
                teleport[idx as usize] += nw;
                seed_sum += nw;
            }
        }
        if seed_sum <= 0.0 {
            return Ok(PprResult { scores: Vec::new(), converged: true, iterations: 0 });
        }
        for t in &mut teleport {
            *t /= seed_sum;
        }
    }

    // Precompute max-weight normalized out-weights per node to avoid overflow/underflow.
    let norm_out: Vec<(f64, f64)> = (0..n)
        .map(|u| match graph.out_weights(u) {
            Some(ws) => {
                let max_w = ws.iter().copied().fold(0.0f64, f64::max);
                if max_w <= 0.0 {
                    (0.0, 0.0)
                } else {
                    let s: f64 = ws.iter().map(|&w| w / max_w).sum();
                    (max_w, s)
                }
            }
            None => (1.0, graph.out(u).len() as f64),
        })
        .collect();

    let mut scores = teleport.clone();
    let mut next = vec![0.0f64; n];
    let mut converged = false;
    let mut ran = 0u32;

    for round in 1..=iterations.max(1) {
        check_cancel(cancel)?;
        ran = round;
        next.fill(0.0);
        let mut dangling = 0.0f64;
        for (u, &sc) in scores.iter().enumerate() {
            let out = graph.out(u);
            let (max_w, sum_w) = norm_out[u];
            if out.is_empty() || sum_w <= 0.0 {
                dangling += sc;
            } else if let Some(ws) = graph.out_weights(u) {
                for (&v, &w) in out.iter().zip(ws.iter()) {
                    next[v as usize] += sc * ((w / max_w) / sum_w);
                }
            } else {
                let share = sc / sum_w;
                for &v in out {
                    next[v as usize] += share;
                }
            }
        }
        let tele_weight = (1.0 - damping) + damping * dangling;
        let mut l1 = 0.0f64;
        for i in 0..n {
            let val = damping * next[i] + tele_weight * teleport[i];
            l1 += (val - scores[i]).abs();
            scores[i] = val;
        }
        if l1 <= tolerance {
            converged = true;
            break;
        }
    }

    let mut out: Vec<(EntityKey, f64)> = graph.keys.iter().copied().zip(scores).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(PprResult { scores: out, converged, iterations: ran })
}

/// #5 Leiden community detection (`leiden-rs 0.8.1`, deterministic single-threaded by `seed`).
pub fn leiden_communities(
    graph: &CsrGraph,
    resolution: f64,
    seed: u64,
    iterations: u32,
) -> Result<Vec<Vec<EntityKey>>> {
    let n = graph.node_count();
    if n == 0 {
        return Ok(Vec::new());
    }
    if !resolution.is_finite() || resolution <= 0.0 {
        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "Leiden resolution must be > 0"));
    }
    let mut builder = leiden_rs::GraphDataBuilder::new(n);
    for u in 0..n {
        let ws = graph.out_weights(u);
        for (idx, &v) in graph.out(u).iter().enumerate() {
            let w = ws.map_or(1.0, |weights| weights[idx]);
            if w > 0.0 && w.is_finite() {
                // Canonicalize `(a <= b)` so mutual directed edges `u->v` & `v->u` merge cleanly in undirected CSR.
                let (a, b) = if u <= v as usize { (u, v as usize) } else { (v as usize, u) };
                builder
                    .add_edge(a, b, w)
                    .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("leiden graph error: {e}")))?;
            }
        }
    }
    let data = builder.build().map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("leiden build error: {e}")))?;
    let config = leiden_rs::LeidenConfig {
        max_iterations: iterations.max(1) as usize,
        resolution,
        seed: Some(seed),
        ..leiden_rs::LeidenConfig::default()
    };
    let found = leiden_rs::Leiden::new(config)
        .run(&data)
        .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("leiden run error: {e}")))?;
    let labels: Vec<u32> = (0..n).map(|v| found.partition.community_of(v) as u32).collect();
    Ok(canonical_groups(&graph.keys, &labels))
}

fn is_entity_live<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    key: EntityKey,
    include_trashed: bool,
    on_entity_read: &mut dyn FnMut(EntityKey),
) -> Result<bool> {
    if include_trashed {
        return Ok(true);
    }
    on_entity_read(key);
    let Some(raw) = txn.get(dbs.entities, &key.to_ne_bytes())? else {
        return Ok(false);
    };
    let (hdr, _) = decode_row_header(raw)?;
    Ok(!hdr.is_trashed())
}

#[allow(clippy::too_many_arguments)]
fn neighbors_set_tracked<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    node: EntityKey,
    rel_kinds: &[RelKindId],
    dir: EdgeDir,
    opts: &GraphBuildOpts<'_>,
    on_adj_read: &mut dyn FnMut(EntityKey, EdgeDir),
    on_entity_read: &mut dyn FnMut(EntityKey),
) -> Result<FxHashSet<EntityKey>> {
    check_cancel(opts.cancel)?;
    on_adj_read(node, dir);
    let mut set = FxHashSet::default();
    if matches!(dir, EdgeDir::Out | EdgeDir::Both) {
        for (dst, _, _) in scan_adj_out_raw(txn, dbs, node, rel_kinds)? {
            if is_entity_live(txn, dbs, dst, opts.include_trashed, on_entity_read)? {
                set.insert(dst);
            }
        }
    }
    if matches!(dir, EdgeDir::In | EdgeDir::Both) {
        for (src, _, _) in scan_adj_in_raw(txn, dbs, node, rel_kinds)? {
            if is_entity_live(txn, dbs, src, opts.include_trashed, on_entity_read)? {
                set.insert(src);
            }
        }
    }
    Ok(set)
}

/// #6 Jaccard similarity of `seed`'s neighborhood against every candidate entity sharing >= 1 neighbor.
pub fn jaccard_similar<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    candidates: Option<&FxHashSet<EntityKey>>,
    rel_kinds: &[RelKindId],
    dir: EdgeDir,
    seed: EntityKey,
    top_k: usize,
) -> Result<Vec<(EntityKey, f64)>> {
    jaccard_similar_tracked(
        txn,
        dbs,
        candidates,
        rel_kinds,
        dir,
        seed,
        top_k,
        &GraphBuildOpts::default(),
        &mut |_, _| {},
        &mut |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
pub fn jaccard_similar_tracked<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    candidates: Option<&FxHashSet<EntityKey>>,
    rel_kinds: &[RelKindId],
    dir: EdgeDir,
    seed: EntityKey,
    top_k: usize,
    opts: &GraphBuildOpts<'_>,
    on_adj_read: &mut dyn FnMut(EntityKey, EdgeDir),
    on_entity_read: &mut dyn FnMut(EntityKey),
) -> Result<Vec<(EntityKey, f64)>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    if !is_entity_live(txn, dbs, seed, opts.include_trashed, on_entity_read)? {
        return Ok(Vec::new());
    }
    let seed_nbrs = neighbors_set_tracked(txn, dbs, seed, rel_kinds, dir, opts, on_adj_read, on_entity_read)?;
    if seed_nbrs.is_empty() {
        return Ok(Vec::new());
    }
    let rev_dir = match dir {
        EdgeDir::Out => EdgeDir::In,
        EdgeDir::In => EdgeDir::Out,
        EdgeDir::Both => EdgeDir::Both,
    };
    let mut approx_bytes = seed_nbrs.len().saturating_mul(16);
    if approx_bytes > opts.max_heap_bytes {
        return Err(ElmdbError::new(ErrorCode::ResourceLimit, "jaccard neighborhood exceeds memory budget"));
    }
    let mut shared_counts: FxHashMap<EntityKey, usize> = FxHashMap::default();
    for &mid in &seed_nbrs {
        for other in neighbors_set_tracked(txn, dbs, mid, rel_kinds, rev_dir, opts, on_adj_read, on_entity_read)? {
            if other == seed {
                continue;
            }
            if candidates.is_some_and(|c| !c.contains(&other)) {
                continue;
            }
            let prev_len = shared_counts.len();
            *shared_counts.entry(other).or_insert(0) += 1;
            if shared_counts.len() > prev_len {
                approx_bytes = approx_bytes.saturating_add(32);
                if approx_bytes > opts.max_heap_bytes {
                    return Err(ElmdbError::new(ErrorCode::ResourceLimit, "jaccard candidates exceed memory budget"));
                }
            }
        }
    }
    let mut out = Vec::with_capacity(shared_counts.len());
    let a = seed_nbrs.len();
    for (other, shared) in shared_counts {
        let b = neighbors_set_tracked(txn, dbs, other, rel_kinds, dir, opts, on_adj_read, on_entity_read)?.len();
        let union = a.saturating_add(b).saturating_sub(shared);
        if union > 0 {
            out.push((other, (shared as f64) / (union as f64)));
        }
    }
    out.sort_by(|x, y| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    out.truncate(top_k);
    Ok(out)
}

/// #7 Adamic-Adar similarity using cross-platform deterministic `libm::log(degree)` and `ExactSum`.
pub fn adamic_adar_similar<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    candidates: Option<&FxHashSet<EntityKey>>,
    rel_kinds: &[RelKindId],
    dir: EdgeDir,
    seed: EntityKey,
    top_k: usize,
) -> Result<Vec<(EntityKey, f64)>> {
    adamic_adar_similar_tracked(
        txn,
        dbs,
        candidates,
        rel_kinds,
        dir,
        seed,
        top_k,
        &GraphBuildOpts::default(),
        &mut |_, _| {},
        &mut |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
pub fn adamic_adar_similar_tracked<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    candidates: Option<&FxHashSet<EntityKey>>,
    rel_kinds: &[RelKindId],
    dir: EdgeDir,
    seed: EntityKey,
    top_k: usize,
    opts: &GraphBuildOpts<'_>,
    on_adj_read: &mut dyn FnMut(EntityKey, EdgeDir),
    on_entity_read: &mut dyn FnMut(EntityKey),
) -> Result<Vec<(EntityKey, f64)>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    if !is_entity_live(txn, dbs, seed, opts.include_trashed, on_entity_read)? {
        return Ok(Vec::new());
    }
    let seed_nbrs = neighbors_set_tracked(txn, dbs, seed, rel_kinds, dir, opts, on_adj_read, on_entity_read)?;
    if seed_nbrs.is_empty() {
        return Ok(Vec::new());
    }
    // Sort `seed_nbrs` ascending by EntityKey so floating-point accumulation order is 100% deterministic!
    let mut sorted_mids: Vec<EntityKey> = seed_nbrs.into_iter().collect();
    sorted_mids.sort_unstable();
    let rev_dir = match dir {
        EdgeDir::Out => EdgeDir::In,
        EdgeDir::In => EdgeDir::Out,
        EdgeDir::Both => EdgeDir::Both,
    };
    let mut approx_bytes = sorted_mids.len().saturating_mul(16);
    if approx_bytes > opts.max_heap_bytes {
        return Err(ElmdbError::new(ErrorCode::ResourceLimit, "adamic-adar neighborhood exceeds memory budget"));
    }
    let mut scores: FxHashMap<EntityKey, ExactSum> = FxHashMap::default();
    for mid in sorted_mids {
        let peers = neighbors_set_tracked(txn, dbs, mid, rel_kinds, rev_dir, opts, on_adj_read, on_entity_read)?;
        let deg = peers.len();
        if deg < 2 {
            continue;
        }
        let contrib = 1.0 / libm::log(deg as f64);
        for other in peers {
            if other == seed {
                continue;
            }
            if candidates.is_some_and(|c| !c.contains(&other)) {
                continue;
            }
            let prev_len = scores.len();
            scores.entry(other).or_default().add(contrib);
            if scores.len() > prev_len {
                approx_bytes = approx_bytes.saturating_add(48);
                if approx_bytes > opts.max_heap_bytes {
                    return Err(ElmdbError::new(
                        ErrorCode::ResourceLimit,
                        "adamic-adar candidates exceed memory budget",
                    ));
                }
            }
        }
    }
    let mut out: Vec<(EntityKey, f64)> = scores.into_iter().map(|(k, sum)| (k, sum.total())).collect();
    out.sort_by(|x, y| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    out.truncate(top_k);
    Ok(out)
}
