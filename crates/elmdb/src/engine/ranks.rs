//! Rank indexes (`docs/roadmap/rank-index.md`, `docs/architecture/schema.md` and `engine.md`): PageRank
//! over a declared graph, kept current by every commit rather than computed by every read.
//!
//! **The system.** Over the index's graph, its nodes the live entities of its types and its arcs the edges of its kinds
//! between two of them, followed its direction, the index holds an estimate `x̂` of the solution of
//! `x = b + α Pᵀ x`, where `b` is `1 − α` at every node and `P` is substochastic: a node's row is each arc's weight over
//! its out-weight `W`, and a node whose arcs weigh nothing has a row of zeros. `rank` sends such a node's walker back to
//! the seeds, every node here, which makes PageRank the normalized solution of this system, so a read divides `x̂` by
//! its sum and the index holds no term for that mass.
//!
//! **The residual.** Each node holds `r = b + α Pᵀ x̂ − x̂` exactly, as every change below keeps it. The estimate is then
//! within `R / (1 − α)` of `x` in L1, where `R = Σ|r|`; divided by their sum `s` as a read divides them, the scores are
//! within `E = 2R / ((1 − α) s)` of PageRank, which a read reports (`error_bound`). A node whose residual is over
//! `θ = tolerance (1 − α)² / (2 + tolerance)` waits in the queue, so once none does, `E` is within the index's tolerance.
//!
//! **Pushing** a node adds its residual to its estimate and spreads `α` of it over its arcs by weight.
//!
//! **A graph change** is local (Zhang, Lofgren and Goel, 2016, with weights): an arc added at `u` keeps `x̂(u) / W(u)`
//! by scaling `x̂(u)` to the new out-weight, so `u`'s other arcs are untouched, and the residuals of `u` and the arc's
//! far end take the difference. A node joins with residual `1 − α` and leaves with its arcs.
//!
//! Every write goes through the unit, so a savepoint rolled back takes the index's changes with it.

use std::collections::BTreeMap;

use crate::error::{Error, ErrorCode, Result};
use crate::schema::{RankDef, Schema};

use super::derive::Violation;
use super::ids;
use super::keys;
use super::relations;
use super::unit::{Read, Txn};
use super::{algorithms, entities};

/// How many nodes a commit pushes per index before leaving the rest to the drain
pub const COMMIT_PUSHES: usize = 16;

/// A rank index's node as stored
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node {
    /// The estimate `x̂`
    pub x: f64,
    /// The residual `r`
    pub r: f64,
    /// The out-weight `W`: its arcs' weights added up
    pub w: f64,
    /// Its type's place among the index's types
    pub type_at: u32,
    /// Its place among the entities
    pub seq: u64,
}

impl Node {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(36);
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.r.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.type_at.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = || Error::new(ErrorCode::Corrupted, "A rank index's node doesn't read");
        if bytes.len() != 36 {
            return Err(corrupt());
        }
        let f = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
        Ok(Node {
            x: f(0),
            r: f(8),
            w: f(16),
            type_at: u32::from_le_bytes(bytes[24..28].try_into().expect("four bytes")),
            seq: u64::from_le_bytes(bytes[28..36].try_into().expect("eight bytes")),
        })
    }
}

/// How far a rank index is built
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Its nodes are being added, a chunk at a time
    Joining,
    /// Every node is in it, and its first pushes aren't done
    Pushing,
    /// Read: its scores are within the bound it reports
    Built,
}

/// A rank index's totals
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Totals {
    pub nodes: u64,
    /// `Σx̂`, what a read divides by
    pub estimates: f64,
    /// `R = Σ|r|`
    pub residuals: f64,
    /// The pushes made since `estimates` and `residuals` were counted from the nodes: each push adds to both, and
    /// rounding with it, so `settle` counts them afresh once there have been as many as nodes, a full count's cost
    /// spread over the pushes that called for it
    pub pushes: u64,
    pub stage: Stage,
    /// Where the sweep of the queue is: the place of the node pushed last, which the next push starts after
    pub swept: Option<(u32, u64)>,
}

impl Totals {
    fn empty() -> Self {
        Totals { nodes: 0, estimates: 0.0, residuals: 0.0, pushes: 0, stage: Stage::Joining, swept: None }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(45);
        out.extend_from_slice(&self.nodes.to_le_bytes());
        out.extend_from_slice(&self.estimates.to_le_bytes());
        out.extend_from_slice(&self.residuals.to_le_bytes());
        out.extend_from_slice(&self.pushes.to_le_bytes());
        out.push(self.stage as u8);
        if let Some((type_at, seq)) = self.swept {
            out.extend_from_slice(&type_at.to_le_bytes());
            out.extend_from_slice(&seq.to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = || Error::new(ErrorCode::Corrupted, "A rank index's totals don't read");
        if bytes.len() != 33 && bytes.len() != 45 {
            return Err(corrupt());
        }
        let swept = (bytes.len() == 45).then(|| {
            (
                u32::from_le_bytes(bytes[33..37].try_into().expect("four bytes")),
                u64::from_le_bytes(bytes[37..45].try_into().expect("eight bytes")),
            )
        });
        let f = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
        let stage = match bytes[32] {
            0 => Stage::Joining,
            1 => Stage::Pushing,
            2 => Stage::Built,
            _ => return Err(corrupt()),
        };
        Ok(Totals {
            nodes: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            estimates: f(8),
            residuals: f(16),
            pushes: u64::from_le_bytes(bytes[24..32].try_into().expect("eight bytes")),
            stage,
            swept,
        })
    }

    /// The bound on the L1 distance of the normalized scores from PageRank, from its residuals, estimates and nodes
    pub fn error(&self, damping: f64) -> f64 {
        error_bound(self.residuals, self.estimates, self.nodes, damping)
    }
}

/// `E = 2R / ((1 − α) s)`, the bound on the L1 distance of an index's normalized scores from PageRank, which its
/// residuals' sum `R` and its estimates' sum `s` give (`docs/evidence/rank-index.md`): the estimates are within
/// `R / (1 − α)` of the solution, and dividing them by `s`, as a read does, at most doubles that over `s`, whatever their
/// signs. 0 for no nodes; none, infinite, for estimates that sum to nothing, which a read can't divide by
pub fn error_bound(residuals: f64, estimates: f64, nodes: u64, damping: f64) -> f64 {
    if nodes == 0 {
        return 0.0;
    }
    if estimates <= 0.0 {
        return f64::INFINITY;
    }
    2.0 * residuals / ((1.0 - damping) * estimates)
}

/// A node's place in its index's score order: its estimate kept to 16 significant bits, so a push that moves the
/// estimate less than that leaves the order's entry as it is. An edge changed moves nearly every estimate a little;
/// rewriting each one's entry would be most of what a drain writes. The order is promised only between scores further
/// apart than twice the bound an index reports, far coarser than this, and a `ranked` read sorts by the estimates
/// themselves
fn order_key(index: &str, node: &Node) -> Vec<u8> {
    keys::rank_order(index, f64::from_bits(order_bits(node.x)), node.type_at, node.seq)
}

/// An estimate's bits as its order key keeps them, the top 28
fn order_bits(x: f64) -> u64 {
    x.to_bits() & !((1u64 << 36) - 1)
}

/// Whether two states of one node have the same place in the score order, read without building either key: most
/// pushes leave it, and a drain writes back nearly every node
fn same_order(a: &Node, b: &Node) -> bool {
    order_bits(a.x) == order_bits(b.x) && (a.type_at, a.seq) == (b.type_at, b.seq)
}

/// What a read reports of a rank index's `nodes` scores: the step each is rounded to, a power of ten no larger than
/// the bound over the nodes, so rounding them all moves the scores at most half the bound again in L1; and the bound
/// with that added, rounded up to one significant figure, still a bound. A score that moved less than the step reads the
/// same, so a watch over it hears a drain only when the drain moved something the step can tell apart
pub fn reported(error: f64, nodes: u64) -> (f64, f64) {
    if error.is_nan() || error <= 0.0 || nodes == 0 {
        return (0.0, 0.0);
    }
    if error.is_infinite() {
        return (error, 0.0);
    }
    let step = 10f64.powf((error / nodes as f64).log10().floor());
    let bound = error + step * nodes as f64 / 2.0;
    let figure = 10f64.powf(bound.log10().floor());
    ((bound / figure).ceil() * figure, step)
}

/// A score rounded to the step a read reports it at
pub fn rounded(score: f64, step: f64) -> f64 {
    if step > 0.0 { (score / step).round() * step } else { score }
}

/// A rank index's totals, or empty ones for an index nothing has written yet
pub fn totals(r: &impl Read, index: &str) -> Result<Totals> {
    match r.get(&keys::rank_totals(index))? {
        Some(bytes) => Totals::decode(&bytes),
        None => Ok(Totals::empty()),
    }
}

/// A rank index's node, when the entity is one
pub fn node(r: &impl Read, index: &str, entity: &str) -> Result<Option<Node>> {
    r.get(&keys::rank_node(index, entity))?.map(|bytes| Node::decode(&bytes)).transpose()
}

/// One rank index's rules, as a change reads them
struct Index<'a> {
    name: &'a str,
    def: &'a RankDef,
    schema: &'a Schema,
    /// The residual over which a node waits to be pushed, `tolerance (1 − α)² / (2 + tolerance)`: with every residual
    /// within it, the estimates sum to at least `(1 − α) n − nθ / (1 − α)`, over which the bound is within the
    /// tolerance (`docs/evidence/rank-index.md`), so once no node waits no push is owed
    threshold: f64,
}

impl<'a> Index<'a> {
    fn new(name: &'a str, def: &'a RankDef, schema: &'a Schema) -> Self {
        let keep = 1.0 - def.damping;
        Index { name, def, schema, threshold: def.tolerance * keep * keep / (2.0 + def.tolerance) }
    }

    /// The residuals' sum at which the bound a read reports is the index's tolerance, over estimates that sum to
    /// `estimates`: pushes stop once the sum is within it, though single nodes are still over the threshold, since only
    /// the sum is what the bound is of
    fn budget(&self, estimates: f64) -> f64 {
        self.def.tolerance * (1.0 - self.def.damping) * estimates.max(0.0) / 2.0
    }

    /// An edge's weight in this index's graph: its own for a kind declared weighted, read with weights; else 1
    fn weight(&self, kind: &str, weight: Option<f64>) -> f64 {
        let weighted = self.def.weights && self.schema.relations.get(kind).is_some_and(|k| k.weighted);
        if weighted { weight.unwrap_or(1.0) } else { 1.0 }
    }

    /// The arcs an edge makes in this index's graph: forward, from its source to its target, backward, or both
    fn arcs(&self) -> &'static [bool] {
        self.def.direction.sides()
    }
}

/// The rank indexes whose graphs `kind`'s edges are in
fn over_kind<'a>(schema: &'a Schema, kind: &'a str) -> impl Iterator<Item = Index<'a>> + 'a {
    schema
        .ranks
        .iter()
        .filter(move |(_, def)| def.relations.iter().any(|k| k == kind))
        .map(|(name, def)| Index::new(name, def, schema))
}

/// The rank indexes whose graphs `entity_type`'s entities are nodes of
fn over_type<'a>(schema: &'a Schema, entity_type: &'a str) -> impl Iterator<Item = Index<'a>> + 'a {
    schema
        .ranks
        .iter()
        .filter(move |(_, def)| def.types.iter().any(|t| t == entity_type))
        .map(|(name, def)| Index::new(name, def, schema))
}

/// The changes one operation makes to one rank index: the nodes it touched, read once and written once, and its totals
struct Change<'i, 'a> {
    index: &'i Index<'a>,
    /// The totals as read, and as they are now
    held: Totals,
    totals: Totals,
    /// Each node as it was when first read, and as it is now; `None` for one that isn't, or is no longer, a node
    nodes: BTreeMap<String, (Option<Node>, Option<Node>)>,
    /// A node leaving, whose edges are still stored: no out-weight counted again counts an arc to it
    leaving: Option<String>,
    /// Of the nodes read, those waiting to be pushed, by place: the queue as it stands for them, where the stored one
    /// stands for the rest
    waiting: BTreeMap<(u32, u64), String>,
}

impl<'i, 'a> Change<'i, 'a> {
    fn begin(t: &impl Read, index: &'i Index<'a>) -> Result<Self> {
        let held = totals(t, index.name)?;
        Ok(Change { index, held, totals: held, nodes: BTreeMap::new(), leaving: None, waiting: BTreeMap::new() })
    }

    fn get(&mut self, t: &impl Read, id: &str) -> Result<Option<Node>> {
        if let Some((_, now)) = self.nodes.get(id) {
            return Ok(*now);
        }
        let held = node(t, self.index.name, id)?;
        self.nodes.insert(id.to_owned(), (held, held));
        if let Some(n) = held
            && n.r.abs() > self.index.threshold
        {
            self.waiting.insert((n.type_at, n.seq), id.to_owned());
        }
        Ok(held)
    }

    fn set(&mut self, t: &impl Read, id: &str, to: Option<Node>) -> Result<()> {
        let from = self.get(t, id)?;
        let estimate = |n: Option<Node>| n.map_or(0.0, |n| n.x);
        let residual = |n: Option<Node>| n.map_or(0.0, |n| n.r.abs());
        self.totals.estimates += estimate(to) - estimate(from);
        self.totals.residuals += residual(to) - residual(from);
        self.nodes.get_mut(id).expect("read above").1 = to;
        if let Some(n) = to.or(from) {
            if to.is_some_and(|n| n.r.abs() > self.index.threshold) {
                self.waiting.insert((n.type_at, n.seq), id.to_owned());
            } else {
                self.waiting.remove(&(n.type_at, n.seq));
            }
        }
        Ok(())
    }

    /// The first node waiting to be pushed after the place `after`, of those this change has read and those it hasn't
    fn next_waiting(&mut self, t: &impl Read, after: Option<(u32, u64)>) -> Result<Option<((u32, u64), String)>> {
        use std::ops::Bound;
        let read = match after {
            Some(place) => self.waiting.range((Bound::Excluded(place), Bound::Unbounded)).next(),
            None => self.waiting.iter().next(),
        }
        .map(|(place, id)| (*place, id.clone()));
        let queue = keys::rank_queue(self.index.name);
        let from =
            after.map_or_else(|| queue.clone(), |(type_at, seq)| keys::rank_queued(self.index.name, type_at, seq));
        let mut stored = None;
        let mut failed = None;
        t.visit_from(&queue, &from, |key, id| {
            let place = match crate::format::key::decode(key)
                .ok()
                .map(|parts| (parts.get(2).cloned(), parts.get(3).cloned()))
            {
                Some((Some(crate::format::key::Part::Number(at)), Some(crate::format::key::Part::Number(seq)))) => {
                    (at as u32, seq as u64)
                }
                _ => {
                    failed =
                        Some(Error::new(ErrorCode::Corrupted, "A rank index's queue holds a key that doesn't read"));
                    return false;
                }
            };
            if Some(place) == after {
                return true;
            }
            let id = String::from_utf8_lossy(id).into_owned();
            // A node this change has read waits as it says, in `waiting`
            if self.nodes.contains_key(&id) {
                return true;
            }
            stored = Some((place, id));
            false
        })?;
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(match (read, stored) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
            (a, b) => a.or(b),
        })
    }

    /// Adds `delta` to a node's residual
    fn add_residual(&mut self, t: &impl Read, id: &str, delta: f64) -> Result<()> {
        if let Some(mut n) = self.get(t, id)? {
            n.r += delta;
            self.set(t, id, Some(n))?;
        }
        Ok(())
    }

    /// Adds an arc of weight `w` from `u` to `v`, both nodes
    fn add_arc(&mut self, t: &impl Read, u: &str, v: &str, w: f64) -> Result<()> {
        let alpha = self.index.def.damping;
        let Some(mut n) = self.get(t, u)? else { return Ok(()) };
        if n.w > 0.0 {
            let f = n.x / n.w;
            let x = f * (n.w + w);
            n.r -= x - n.x;
            n.x = x;
            n.w += w;
            self.set(t, u, Some(n))?;
            self.add_residual(t, v, alpha * f * w)?;
        } else if w > 0.0 {
            // A node whose arcs weighed nothing sent none of its estimate along them; this arc now carries all of it
            n.w = w;
            let x = n.x;
            self.set(t, u, Some(n))?;
            self.add_residual(t, v, alpha * x)?;
        }
        Ok(())
    }

    /// Removes an arc of weight `w` from `u` to `v`, both nodes, the edge no longer stored or `v` leaving
    fn remove_arc(&mut self, t: &impl Read, u: &str, v: &str, w: f64) -> Result<()> {
        self.reweigh_arc(t, u, v, w, 0.0)
    }

    /// Changes an arc's weight from `from` to `to`, its edge already stored with the new one: `u`'s estimate scaled to
    /// its new out-weight, and `v`'s residual the change in what the arc carries. The out-weight is counted again from
    /// the stored arcs when subtracting would cancel to a rounding error of nothing
    fn reweigh_arc(&mut self, t: &impl Read, u: &str, v: &str, from: f64, to: f64) -> Result<()> {
        let alpha = self.index.def.damping;
        let Some(mut n) = self.get(t, u)? else { return Ok(()) };
        if n.w <= 0.0 {
            // Its arcs carried nothing; this one now carries all of it, as `add_arc` has it
            return if to > 0.0 { self.add_arc(t, u, v, to) } else { Ok(()) };
        }
        let f = n.x / n.w;
        let mut left = n.w - from + to;
        if left <= n.w * 1e-9 {
            left = self.out_weight(t, u)?;
        }
        if left > 0.0 {
            let x = f * left;
            n.r -= x - n.x;
            n.x = x;
        }
        n.w = left;
        self.set(t, u, Some(n))?;
        self.add_residual(t, v, alpha * f * (to - from))
    }

    /// A node's arcs to other nodes in this index's graph, as they are stored now: the far end and the weight
    fn arcs_of(&mut self, t: &impl Read, u: &str) -> Result<Vec<(String, f64)>> {
        let mut found = Vec::new();
        for kind in &self.index.def.relations {
            for &outward in self.index.def.direction.sides() {
                relations::visit_far_ends(t, u, outward, Some(kind), |far, weight| {
                    found.push((far.to_owned(), self.index.weight(kind, weight)));
                    Ok(true)
                })?;
            }
        }
        let mut arcs = Vec::with_capacity(found.len());
        for (far, weight) in found {
            if self.leaving.as_deref() != Some(far.as_str()) && self.get(t, &far)?.is_some() {
                arcs.push((far, weight));
            }
        }
        Ok(arcs)
    }

    fn out_weight(&mut self, t: &impl Read, u: &str) -> Result<f64> {
        Ok(self.arcs_of(t, u)?.iter().map(|(_, w)| w).sum())
    }

    /// Every edge touching `v` in this index's kinds whose other end is a node, as the arcs it makes: `(from, to, w)`
    fn arcs_touching(&mut self, t: &impl Read, v: &str) -> Result<Vec<(String, String, f64)>> {
        let mut edges = Vec::new();
        for kind in &self.index.def.relations {
            for outward in [true, false] {
                relations::visit_far_ends(t, v, outward, Some(kind), |far, weight| {
                    edges.push((outward, far.to_owned(), self.index.weight(kind, weight)));
                    Ok(true)
                })?;
            }
        }
        let mut arcs = Vec::new();
        for (outward, far, weight) in edges {
            if self.get(t, &far)?.is_none() {
                continue;
            }
            let (source, target) = if outward { (v.to_owned(), far) } else { (far, v.to_owned()) };
            for &forward in self.index.arcs() {
                let (from, to) = if forward { (&source, &target) } else { (&target, &source) };
                arcs.push((from.clone(), to.clone(), weight));
            }
        }
        Ok(arcs)
    }

    /// Adds `id` as a node, with each of its arcs to a node
    fn join(&mut self, t: &impl Read, id: &str, type_at: u32, seq: u64) -> Result<()> {
        if self.get(t, id)?.is_some() {
            return Ok(());
        }
        let keep = 1.0 - self.index.def.damping;
        self.set(t, id, Some(Node { x: 0.0, r: keep, w: 0.0, type_at, seq }))?;
        self.totals.nodes += 1;
        for (from, to, weight) in self.arcs_touching(t, id)? {
            self.add_arc(t, &from, &to, weight)?;
        }
        Ok(())
    }

    /// Removes `id`'s arcs, then the node
    fn leave(&mut self, t: &impl Read, id: &str) -> Result<()> {
        if self.get(t, id)?.is_none() {
            return Ok(());
        }
        let arcs = self.arcs_touching(t, id)?;
        self.leaving = Some(id.to_owned());
        for (from, to, weight) in arcs {
            self.remove_arc(t, &from, &to, weight)?;
        }
        self.leaving = None;
        self.set(t, id, None)?;
        self.totals.nodes -= 1;
        Ok(())
    }

    /// Pushes a node: its residual into its estimate, and `α` of it along its arcs by weight
    fn push(&mut self, t: &impl Read, id: &str) -> Result<()> {
        let Some(mut n) = self.get(t, id)? else { return Ok(()) };
        let residual = n.r;
        if n.w > 0.0 {
            let share = self.index.def.damping * residual / n.w;
            for (far, weight) in self.arcs_of(t, id)? {
                self.add_residual(t, &far, share * weight)?;
            }
        }
        n.x += residual;
        n.r = 0.0;
        self.set(t, id, Some(n))
    }

    /// Writes what changed: each node, its place in score order and in the queue, and the totals
    fn finish(self, t: &mut Txn<'_, '_>) -> Result<()> {
        if self.totals == self.held && self.nodes.values().all(|(before, after)| before == after) {
            return Ok(());
        }
        let name = self.index.name;
        let threshold = self.index.threshold;
        for (id, (before, after)) in &self.nodes {
            if before == after {
                continue;
            }
            if let Some(b) = before {
                if after.is_none_or(|a| !same_order(&a, b)) {
                    t.del(&order_key(name, b))?;
                }
                if b.r.abs() > threshold && after.is_none_or(|a| a.r.abs() <= threshold) {
                    t.del(&keys::rank_queued(name, b.type_at, b.seq))?;
                }
            }
            match after {
                Some(a) => {
                    t.put(&keys::rank_node(name, id), &a.encode())?;
                    if before.is_none_or(|b| !same_order(a, &b)) {
                        t.put(&order_key(name, a), id.as_bytes())?;
                    }
                    if a.r.abs() > threshold && before.is_none_or(|b| b.r.abs() <= threshold) {
                        t.put(&keys::rank_queued(name, a.type_at, a.seq), id.as_bytes())?;
                    }
                }
                None => t.del(&keys::rank_node(name, id))?,
            }
        }
        t.put(&keys::rank_totals(name), &self.totals.encode())
    }
}

/// The node an entity would be: its type's place among the index's and its place, when it is live and of one of them
fn would_be(t: &impl Read, index: &Index<'_>, id: &str) -> Result<Option<(u32, u64)>> {
    let entity_type = ids::type_of(id);
    let Some(at) = index.def.types.iter().position(|ty| ty == entity_type) else { return Ok(None) };
    Ok(entities::record(t, id)?.filter(|r| !r.trashed).map(|r| (at as u32, r.seq)))
}

/// An edge of `kind` from `source` to `target` changed: `before` is the weight it had, `None` when it wasn't stored, and
/// `after` the weight it has, `None` when it is gone. Called once the edge's entries say so
pub fn edge_changed(
    t: &mut Txn<'_, '_>,
    kind: &str,
    source: &str,
    target: &str,
    before: Option<Option<f64>>,
    after: Option<Option<f64>>,
) -> Result<()> {
    let schema = t.schema;
    for index in over_kind(schema, kind) {
        let mut change = Change::begin(t, &index)?;
        if change.get(t, source)?.is_none() || change.get(t, target)?.is_none() {
            continue;
        }
        for &forward in index.arcs() {
            let (from, to) = if forward { (source, target) } else { (target, source) };
            match (before, after) {
                (Some(old), Some(new)) => {
                    change.reweigh_arc(t, from, to, index.weight(kind, old), index.weight(kind, new))?
                }
                (Some(old), None) => change.remove_arc(t, from, to, index.weight(kind, old))?,
                (None, Some(new)) => change.add_arc(t, from, to, index.weight(kind, new))?,
                (None, None) => {}
            }
        }
        change.finish(t)?;
    }
    Ok(())
}

/// An entity became live (created, restored) or stopped being (trashed, destroyed): it joins or leaves the graph of
/// every rank index over its type. Called once its record says so; a destroyed entity's edges are gone already
pub fn liveness(t: &mut Txn<'_, '_>, id: &str, live: bool) -> Result<()> {
    let schema = t.schema;
    for index in over_type(schema, ids::type_of(id)) {
        let mut change = Change::begin(t, &index)?;
        match (live, would_be(t, &index, id)?) {
            (true, Some((type_at, seq))) => change.join(t, id, type_at, seq)?,
            (false, _) => change.leave(t, id)?,
            (true, None) => continue,
        }
        change.finish(t)?;
    }
    Ok(())
}

/// Pushes up to `limit` of the nodes waiting in a rank index's queue, sweeping it in place order: each push takes the
/// next waiting node after the one before, from the queue's start again once past its end. Pushing the first waiting
/// node each time instead lets a hub and the node before it hand residual back and forth while the nodes after wait,
/// the work growing geometrically with them. Whether any still wait
pub fn push(t: &mut Txn<'_, '_>, index: &str, limit: usize) -> Result<bool> {
    let schema = t.schema;
    let Some(def) = schema.ranks.get(index) else { return Ok(false) };
    let index = Index::new(index, def, schema);
    // One change for every push: each node read and written once, however many pushes touch it
    let mut change = Change::begin(t, &index)?;
    let mut after = change.totals.swept;
    for _ in 0..limit {
        if change.totals.residuals <= index.budget(change.totals.estimates) {
            break;
        }
        let next = match change.next_waiting(t, after)? {
            None if after.is_some() => change.next_waiting(t, None)?,
            next => next,
        };
        let Some((place, id)) = next else { break };
        change.push(t, &id)?;
        change.totals.pushes += 1;
        after = Some(place);
    }
    change.totals.swept = after;
    let waiting = change.totals.residuals > index.budget(change.totals.estimates);
    change.finish(t)?;
    Ok(waiting)
}

/// Drains a rank index in memory: the nodes waiting to be pushed, read from its queue, pushed in the sweep `push`
/// makes until none waits or `limit` pushes are made, each node and its arcs read once as the pushes first reach it,
/// and the nodes whose estimate or residual moved written back once. A drain costs the nodes its pushes reach, not the
/// graph: a change whose residual spreads to a few hundred nodes reads and writes those. Whether any still wait
pub fn drain(t: &mut Txn<'_, '_>, index: &str, limit: usize) -> Result<bool> {
    let schema = t.schema;
    let Some(def) = schema.ranks.get(index) else { return Ok(false) };
    let index = Index::new(index, def, schema);
    let held = totals(t, index.name)?;
    // Arcs through the builder's edge mirror when it has one, which holds what the store does at this transaction's
    // sequence
    let mirror = t.state.mirror.clone();
    let mut graph = mirror.as_ref().map(|m| m.lock());
    if let Some(graph) = graph.as_mut() {
        graph.at(super::mirror::stored_sequence(t)?);
    }
    let mut reached = Reached::default();
    // A drain reaching a quarter of the nodes reads them all in one pass, cheaper than one read each. A drain stops once
    // the residuals' sum is within its budget, so the queue it leaves can be most of the nodes
    let most = held.nodes / 4;
    let mut queued = 0u64;
    t.visit(&keys::rank_queue(index.name), |_, _| {
        queued += 1;
        queued <= most
    })?;
    if queued > most {
        reached.read_all(t, &index)?;
    } else {
        // Every node waiting is queued, so reading the queue's reaches all of them; a node read later waits only if a
        // push here put it over the threshold
        let mut ids = Vec::new();
        t.visit(&keys::rank_queue(index.name), |_, id| {
            ids.push(String::from_utf8_lossy(id).into_owned());
            true
        })?;
        for id in &ids {
            reached.node(t, &index, id)?;
        }
    }
    let threshold = index.threshold;
    let alpha = def.damping;
    let mut residuals = held.residuals;
    let mut estimates = held.estimates;
    let mut after = held.swept;
    // The node pushed last, while every node is read
    let mut last: Option<usize> = None;
    let mut pushes = 0;
    while pushes < limit && residuals > index.budget(estimates) {
        if !reached.all && reached.nodes.len() as u64 > most {
            reached.read_all(t, &index)?;
            // Numbered again: the sweep takes up after the place pushed last
            last = None;
        }
        // The next waiting node after the one pushed last, from the queue's start again once past its end
        let u = if reached.all {
            let from = match (last, after) {
                (Some(u), _) => u + 1,
                (None, Some(place)) => reached.nodes.partition_point(|n| (n.type_at, n.seq) <= place),
                (None, None) => 0,
            };
            match reached.waiting.from(0, from as u64).or_else(|| reached.waiting.first()) {
                Some((_, u)) => u as usize,
                None => break,
            }
        } else {
            match after.and_then(|place| reached.waiting.after(place)).or_else(|| reached.waiting.first()) {
                Some(place) => reached.by_place[&place],
                None => break,
            }
        };
        let place = (reached.nodes[u].type_at, reached.nodes[u].seq);
        let arcs = reached.arcs(t, &index, graph.as_deref_mut(), u)?;
        let residual = reached.nodes[u].r;
        residuals -= residual.abs();
        if reached.nodes[u].w > 0.0 {
            let share = alpha * residual / reached.nodes[u].w;
            for &(v, weight) in &arcs {
                let node = &mut reached.nodes[v];
                let held = node.r.abs();
                node.r += share * weight;
                let now = node.r.abs();
                residuals += now - held;
                if (now > threshold) != (held > threshold) {
                    let key = if reached.all { (0, v as u64) } else { (node.type_at, node.seq) };
                    reached.waiting.set(key, now > threshold);
                }
            }
        }
        reached.arcs[u] = Some(arcs);
        reached.nodes[u].x += residual;
        estimates += residual;
        reached.nodes[u].r = 0.0;
        reached.waiting.set(if reached.all { (0, u as u64) } else { place }, false);
        after = Some(place);
        last = Some(u);
        pushes += 1;
    }
    drop(graph);
    // Owed pushes: past the budget with a node to push, so a drain never waits on a sum no push can lower
    let waiting = residuals > index.budget(estimates) && reached.waiting.count > 0;
    // Written back: the nodes that moved, their places in score order and in the queue, and the totals
    for (at, (old, new)) in reached.before.iter().zip(&reached.nodes).enumerate() {
        if old == new {
            continue;
        }
        let id = &reached.ids[at];
        t.put(&keys::rank_node(index.name, id), &new.encode())?;
        if !same_order(old, new) {
            t.del(&order_key(index.name, old))?;
            t.put(&order_key(index.name, new), id.as_bytes())?;
        }
        let (was, is) = (old.r.abs() > threshold, new.r.abs() > threshold);
        if was && !is {
            t.del(&keys::rank_queued(index.name, old.type_at, old.seq))?;
        } else if is && !was {
            t.put(&keys::rank_queued(index.name, new.type_at, new.seq), id.as_bytes())?;
        }
    }
    let totals =
        Totals { estimates, residuals, pushes: held.pushes + pushes as u64, swept: after.or(held.swept), ..held };
    t.put(&keys::rank_totals(index.name), &totals.encode())?;
    Ok(waiting)
}

/// The nodes a drain's pushes have reached, each read once: as read, as pushed, and its arcs once it is pushed
#[derive(Default)]
struct Reached {
    /// Each entity read, and its place here when it is a node
    at: rustc_hash::FxHashMap<String, Option<usize>>,
    /// Each node read, by its place in the index
    by_place: rustc_hash::FxHashMap<(u32, u64), usize>,
    ids: Vec<String>,
    before: Vec<Node>,
    nodes: Vec<Node>,
    /// A node's arcs to other nodes, `(node, weight)`, read the first time it is pushed
    arcs: Vec<Option<Vec<(usize, f64)>>>,
    /// The nodes reached whose residual is over the threshold, every node waiting, the queue having been read: by their
    /// places, or once every node is read and numbered in place order, by their numbers here
    waiting: Places,
    /// Whether every node is read
    all: bool,
}

impl Reached {
    /// The entity's place here, read from the store the first time; `None` when it isn't a node
    fn node(&mut self, t: &impl Read, index: &Index<'_>, id: &str) -> Result<Option<usize>> {
        if let Some(&at) = self.at.get(id) {
            return Ok(at);
        }
        let read = if self.all { None } else { node(t, index.name, id)? };
        Ok(match read {
            Some(n) => Some(self.add(index, id.to_owned(), n)),
            None => {
                self.at.insert(id.to_owned(), None);
                None
            }
        })
    }

    fn add(&mut self, index: &Index<'_>, id: String, n: Node) -> usize {
        let at = self.nodes.len();
        self.at.insert(id.clone(), Some(at));
        if !self.all {
            self.by_place.insert((n.type_at, n.seq), at);
        }
        self.ids.push(id);
        self.before.push(n);
        self.nodes.push(n);
        self.arcs.push(None);
        if n.r.abs() > index.threshold {
            self.waiting.set((n.type_at, n.seq), true);
        }
        at
    }

    /// Every node not yet read, in one pass over the index's nodes; an entity read after this that isn't here is no node
    fn read_all(&mut self, t: &impl Read, index: &Index<'_>) -> Result<()> {
        let mut read = Vec::new();
        let mut failed = None;
        t.visit(&keys::rank_nodes(index.name), |key, bytes| {
            let id = match crate::format::key::decode(key).ok().and_then(|p| p.into_iter().nth(2)) {
                Some(crate::format::key::Part::String(id)) => id,
                _ => {
                    failed = Some(Error::new(ErrorCode::Corrupted, "A rank index's node key doesn't read"));
                    return false;
                }
            };
            if self.at.contains_key(&id) {
                return true;
            }
            match Node::decode(bytes) {
                Ok(n) => {
                    read.push((id, n));
                    true
                }
                Err(e) => {
                    failed = Some(e);
                    false
                }
            }
        })?;
        if let Some(e) = failed {
            return Err(e);
        }
        self.all = true;
        // Swept by number from here on, so no node is looked up by its place
        self.by_place = Default::default();
        self.at.reserve(read.len());
        self.ids.reserve(read.len());
        self.before.reserve(read.len());
        self.nodes.reserve(read.len());
        self.arcs.reserve(read.len());
        for (id, n) in read {
            self.add(index, id, n);
        }
        self.in_place_order(index);
        Ok(())
    }

    /// Every node numbered again in place order, the sweep's, so pushes sweeping it read the nodes and their arcs in
    /// the order they are stored here
    fn in_place_order(&mut self, index: &Index<'_>) {
        let mut order: Vec<usize> = (0..self.nodes.len()).collect();
        order.sort_unstable_by_key(|&at| (self.nodes[at].type_at, self.nodes[at].seq));
        let mut renumbered = vec![0; order.len()];
        for (to, &from) in order.iter().enumerate() {
            renumbered[from] = to;
        }
        let mut ids = std::mem::take(&mut self.ids);
        let mut arcs = std::mem::take(&mut self.arcs);
        self.ids = order.iter().map(|&at| std::mem::take(&mut ids[at])).collect();
        self.before = order.iter().map(|&at| self.before[at]).collect();
        self.nodes = order.iter().map(|&at| self.nodes[at]).collect();
        self.arcs = order
            .iter()
            .map(|&at| arcs[at].take().map(|list| list.into_iter().map(|(v, w)| (renumbered[v], w)).collect()))
            .collect();
        for at in self.at.values_mut().flatten() {
            *at = renumbered[*at];
        }
        self.waiting = Places::default();
        for (at, n) in self.nodes.iter().enumerate() {
            if n.r.abs() > index.threshold {
                self.waiting.set((0, at as u64), true);
            }
        }
    }

    /// A node's arcs, as `Change::arcs_of` reads them: kind by kind, each side, in stored order, those to a node; taken
    /// out for the push, which puts them back
    fn arcs(
        &mut self,
        t: &impl Read,
        index: &Index<'_>,
        mut graph: Option<&mut super::mirror::EdgeMirror>,
        u: usize,
    ) -> Result<Vec<(usize, f64)>> {
        if let Some(arcs) = self.arcs[u].take() {
            return Ok(arcs);
        }
        // A far end already read is taken by its place here; one not read yet is named, read once the visit is done
        enum Far {
            Read(usize, f64),
            Unread(String, f64),
        }
        let id = std::mem::take(&mut self.ids[u]);
        if self.all {
            // Every node is read: a far end not here is no node
            let mut arcs = Vec::new();
            for kind in &index.def.relations {
                for &outward in index.def.direction.sides() {
                    let at = &self.at;
                    let mut arc = |far: &str, weight: Option<f64>| {
                        if let Some(Some(v)) = at.get(far) {
                            arcs.push((*v, index.weight(kind, weight)));
                        }
                    };
                    let visited = match graph.as_deref_mut() {
                        Some(graph) => graph.visit_far_ends(t, &id, outward, kind, arc),
                        None => relations::visit_far_ends(t, &id, outward, Some(kind), |far, weight| {
                            arc(far, weight);
                            Ok(true)
                        }),
                    };
                    if let Err(e) = visited {
                        self.ids[u] = id;
                        return Err(e);
                    }
                }
            }
            self.ids[u] = id;
            return Ok(arcs);
        }
        let mut found: Vec<Far> = Vec::new();
        for kind in &index.def.relations {
            for &outward in index.def.direction.sides() {
                let at = &self.at;
                let all = self.all;
                let mut arc = |far: &str, weight: Option<f64>| {
                    let weight = index.weight(kind, weight);
                    match at.get(far) {
                        Some(Some(v)) => found.push(Far::Read(*v, weight)),
                        Some(None) => {}
                        None if all => {}
                        None => found.push(Far::Unread(far.to_owned(), weight)),
                    }
                };
                let visited = match graph.as_deref_mut() {
                    Some(graph) => graph.visit_far_ends(t, &id, outward, kind, arc),
                    None => relations::visit_far_ends(t, &id, outward, Some(kind), |far, weight| {
                        arc(far, weight);
                        Ok(true)
                    }),
                };
                if let Err(e) = visited {
                    self.ids[u] = id;
                    return Err(e);
                }
            }
        }
        self.ids[u] = id;
        let mut arcs = Vec::with_capacity(found.len());
        for far in found {
            match far {
                Far::Read(v, weight) => arcs.push((v, weight)),
                Far::Unread(far, weight) => {
                    if let Some(v) = self.node(t, index, &far)? {
                        arcs.push((v, weight));
                    }
                }
            }
        }
        Ok(arcs)
    }
}

/// A set of places, a bit per entity place under each type's: what a drain's sweep takes the next waiting node from,
/// a word at a time
#[derive(Default)]
struct Places {
    bits: Vec<Vec<u64>>,
    count: usize,
}

impl Places {
    #[inline]
    fn set(&mut self, (type_at, seq): (u32, u64), on: bool) {
        let (t, word, bit) = (type_at as usize, (seq / 64) as usize, seq % 64);
        if self.bits.len() <= t {
            self.bits.resize_with(t + 1, Vec::new);
        }
        let words = &mut self.bits[t];
        if words.len() <= word {
            if !on {
                return;
            }
            words.resize(word + 1, 0);
        }
        let held = words[word] & (1 << bit) != 0;
        if held != on {
            words[word] ^= 1 << bit;
            if on {
                self.count += 1;
            } else {
                self.count -= 1;
            }
        }
    }

    /// The first place at or after `seq` under type `t` and after it under the types following
    #[inline]
    fn from(&self, type_at: u32, seq: u64) -> Option<(u32, u64)> {
        for t in type_at as usize..self.bits.len() {
            let start = if t == type_at as usize { seq } else { 0 };
            let words = &self.bits[t];
            let mut word = (start / 64) as usize;
            if word >= words.len() {
                continue;
            }
            let mut w = words[word] & (u64::MAX << (start % 64));
            loop {
                if w != 0 {
                    return Some((t as u32, word as u64 * 64 + u64::from(w.trailing_zeros())));
                }
                word += 1;
                if word >= words.len() {
                    break;
                }
                w = words[word];
            }
        }
        None
    }

    #[inline]
    fn first(&self) -> Option<(u32, u64)> {
        if self.count == 0 { None } else { self.from(0, 0) }
    }

    #[inline]
    fn after(&self, (type_at, seq): (u32, u64)) -> Option<(u32, u64)> {
        if self.count == 0 {
            return None;
        }
        match seq.checked_add(1) {
            Some(next) => self.from(type_at, next),
            None => self.from(type_at + 1, 0),
        }
    }
}

/// A commit's own pushes, up to `COMMIT_PUSHES` per rank index; the indexes left with nodes waiting, for the drain
pub fn push_at_commit(t: &mut Txn<'_, '_>) -> Result<Vec<String>> {
    let names: Vec<String> = t.schema.ranks.keys().cloned().collect();
    let mut waiting = Vec::new();
    for name in names {
        if push(t, &name, COMMIT_PUSHES)? {
            waiting.push(name);
        }
    }
    Ok(waiting)
}

/// One chunk of a rank index's build: adds up to `limit` of its types' live entities after `after` (a type's place among
/// the index's and an entity's place) as nodes, those already nodes skipped. The last one added, or `None` once every
/// one is a node, when the index moves on to pushing
pub fn join_chunk(
    t: &mut Txn<'_, '_>,
    index: &str,
    after: Option<(u32, u64)>,
    limit: usize,
) -> Result<Option<(u32, u64)>> {
    let schema = t.schema;
    let Some(def) = schema.ranks.get(index) else { return Ok(None) };
    let index = Index::new(index, def, schema);
    let mut taken = Vec::new();
    for (at, entity_type) in def.types.iter().enumerate() {
        let at = at as u32;
        if after.is_some_and(|(a, _)| at < a) {
            continue;
        }
        let range = keys::type_range(false, entity_type);
        let from = match after {
            Some((a, seq)) if a == at => keys::of_type(false, entity_type, seq + 1),
            _ => range.clone(),
        };
        t.visit_from(&range, &from, |key, id| {
            let seq = match crate::format::key::decode(key).ok().and_then(|parts| parts.into_iter().nth(2)) {
                Some(crate::format::key::Part::Number(n)) => n as u64,
                _ => return false,
            };
            taken.push((at, seq, id.to_vec()));
            taken.len() < limit
        })?;
        if taken.len() >= limit {
            break;
        }
    }
    let mut change = Change::begin(t, &index)?;
    for (at, seq, id) in &taken {
        let id = super::values::decode(id)?;
        let id = id.as_str().ok_or_else(|| Error::new(ErrorCode::Corrupted, "A type's entry names no entity"))?;
        change.join(t, id, *at, *seq)?;
    }
    let last = taken.last().map(|(at, seq, _)| (*at, *seq));
    if taken.len() < limit {
        change.totals.stage = change.totals.stage.max(Stage::Pushing);
    }
    change.finish(t)?;
    Ok(if taken.len() < limit { None } else { last })
}

/// After a drain empties the queue: the index built, and its totals counted again from the nodes when its first build
/// ends or as many pushes as it has nodes have added their rounding since the last count
pub fn settle(t: &mut Txn<'_, '_>, index: &str) -> Result<()> {
    let mut totals = totals(t, index)?;
    if totals.stage == Stage::Built && totals.pushes < totals.nodes {
        return Ok(());
    }
    let (mut nodes, mut estimates, mut residuals) = (0u64, 0.0, 0.0);
    let mut failed = None;
    t.visit(&keys::rank_nodes(index), |_, bytes| match Node::decode(bytes) {
        Ok(n) => {
            nodes += 1;
            estimates += n.x;
            residuals += n.r.abs();
            true
        }
        Err(e) => {
            failed = Some(e);
            false
        }
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    totals.nodes = nodes;
    totals.estimates = estimates;
    totals.residuals = residuals;
    totals.pushes = 0;
    if totals.stage == Stage::Pushing {
        totals.stage = Stage::Built;
    }
    t.put(&keys::rank_totals(index), &totals.encode())
}

/// Deletes a rank index's data
pub fn clear(t: &mut Txn<'_, '_>, index: &str) -> Result<()> {
    t.clear_prefix(&keys::rank_nodes(index))?;
    t.clear_prefix(&keys::rank_orders(index))?;
    t.clear_prefix(&keys::rank_queue(index))?;
    t.del(&keys::rank_totals(index))
}

/// Builds a rank index in the unit, from nothing: every node joined, then pushed until none waits
pub fn rebuild(t: &mut Txn<'_, '_>, index: &str) -> Result<()> {
    clear(t, index)?;
    let mut after = None;
    while let Some(last) = join_chunk(t, index, after, 1000)? {
        after = Some(last);
    }
    while drain(t, index, usize::MAX)? {}
    settle(t, index)
}

/// Whether a rank index is built, or still adding nodes or doing its first pushes
pub fn is_built(r: &impl Read, index: &str) -> Result<bool> {
    Ok(totals(r, index)?.stage == Stage::Built)
}

/// The rank indexes not built yet, or whose bound is past their tolerance, for the drain
pub fn unfinished(r: &impl Read) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (name, def) in &r.schema().ranks {
        let totals = totals(r, name)?;
        // Past the tolerance with a node queued to push: the drain's own test of whether it owes pushes
        let queued = || -> Result<bool> {
            let mut any = false;
            r.visit(&keys::rank_queue(name), |_, _| {
                any = true;
                false
            })?;
            Ok(any)
        };
        if totals.stage != Stage::Built || (totals.error(def.damping) > def.tolerance && queued()?) {
            out.push(name.clone());
        }
    }
    Ok(out)
}

/// What `check()` holds a rank index to: the contract, its scores within the bound it reports of `rank`'s over the same
/// graph; the invariant, each residual and out-weight what the estimates and the edges make them; the score order and
/// the queue exactly what the nodes make them; and the totals what the nodes add up to
pub fn violations(r: &impl Read) -> Result<Vec<Violation>> {
    let mut out = Vec::new();
    for (name, def) in &r.schema().ranks {
        let index = Index::new(name, def, r.schema());
        let totals = totals(r, name)?;
        let violation = |message: String| Violation { subject: format!("ranks.{name}"), message };
        let mut nodes: Vec<(String, Node)> = Vec::new();
        let mut failed = None;
        r.visit(&keys::rank_nodes(name), |key, bytes| {
            let id = match crate::format::key::decode(key).ok().and_then(|p| p.into_iter().nth(2)) {
                Some(crate::format::key::Part::String(id)) => id,
                _ => return true,
            };
            match Node::decode(bytes) {
                Ok(n) => nodes.push((id, n)),
                Err(e) => failed = Some(e),
            }
            failed.is_none()
        })?;
        if let Some(e) = failed {
            return Err(e);
        }
        if totals.stage == Stage::Joining {
            continue;
        }
        let place: BTreeMap<&str, usize> = nodes.iter().enumerate().map(|(at, (id, _))| (id.as_str(), at)).collect();
        // The nodes are every live entity of the types
        let mut expected = 0u64;
        for entity_type in &def.types {
            r.visit(&keys::type_range(false, entity_type), |_, _| {
                expected += 1;
                true
            })?;
        }
        if expected != nodes.len() as u64 {
            out.push(violation(format!("holds {} nodes where its types have {expected} live entities", nodes.len())));
            continue;
        }
        // Each node's arcs, as the edges make them
        let mut arcs: Vec<Vec<(usize, f64)>> = vec![Vec::new(); nodes.len()];
        for (at, (id, _)) in nodes.iter().enumerate() {
            for kind in &def.relations {
                for &outward in def.direction.sides() {
                    relations::visit_far_ends(r, id, outward, Some(kind), |far, weight| {
                        if let Some(&to) = place.get(far) {
                            arcs[at].push((to, index.weight(kind, weight)));
                        }
                        Ok(true)
                    })?;
                }
            }
        }
        let alpha = def.damping;
        let keep = 1.0 - alpha;
        let mut inflow = vec![0.0; nodes.len()];
        for (at, (_, n)) in nodes.iter().enumerate() {
            // No comparison below is true of NaN, so a value that isn't finite would pass every one of them
            if !(n.x.is_finite() && n.r.is_finite() && n.w.is_finite()) {
                out.push(violation(format!(
                    "{} holds an estimate, residual or out-weight that isn't finite",
                    nodes[at].0
                )));
            }
            let w: f64 = arcs[at].iter().map(|(_, w)| w).sum();
            if (w - n.w).abs() > 1e-9 * w.max(1.0) {
                out.push(violation(format!("{} holds out-weight {} where its arcs weigh {w}", nodes[at].0, n.w)));
            }
            if w > 0.0 {
                for &(to, weight) in &arcs[at] {
                    inflow[to] += alpha * n.x * weight / w;
                }
            }
        }
        let scale = nodes.iter().map(|(_, n)| n.x.abs()).fold(1.0, f64::max);
        for (at, (id, n)) in nodes.iter().enumerate() {
            let residual = keep + inflow[at] - n.x;
            if (residual - n.r).abs() > 1e-9 * scale {
                out.push(violation(format!("{id} holds residual {} where its estimates make it {residual}", n.r)));
            }
        }
        // The score order and the queue
        let mut orders = 0usize;
        let mut queued = 0usize;
        r.visit(&keys::rank_orders(name), |_, _| {
            orders += 1;
            true
        })?;
        r.visit(&keys::rank_queue(name), |_, _| {
            queued += 1;
            true
        })?;
        let mut expected_queued = 0usize;
        for (id, n) in &nodes {
            // An estimate that isn't finite has no place in the score order, and is reported above
            if n.x.is_finite() && !r.contains(&order_key(name, n))? {
                out.push(violation(format!("{id} is missing from the score order")));
            }
            if n.r.abs() > index.threshold {
                expected_queued += 1;
                if !r.contains(&keys::rank_queued(name, n.type_at, n.seq))? {
                    out.push(violation(format!("{id} waits to be pushed and isn't queued")));
                }
            }
        }
        if orders != nodes.len() {
            out.push(violation(format!("its score order holds {orders} entries for {} nodes", nodes.len())));
        }
        if queued != expected_queued {
            out.push(violation(format!("its queue holds {queued} nodes where {expected_queued} wait")));
        }
        // The totals
        let estimates: f64 = nodes.iter().map(|(_, n)| n.x).sum();
        let residuals: f64 = nodes.iter().map(|(_, n)| n.r.abs()).sum();
        if totals.nodes != nodes.len() as u64
            || (totals.estimates - estimates).abs() > 1e-9 * estimates.abs().max(1.0)
            || (totals.residuals - residuals).abs() > 1e-9 * estimates.abs().max(1.0)
        {
            out.push(violation(format!(
                "its totals ({} nodes, estimates {}, residuals {}) aren't its nodes' ({}, {estimates}, {residuals})",
                totals.nodes,
                totals.estimates,
                totals.residuals,
                nodes.len()
            )));
        }
        // The contract: within the reported bound of `rank` over the same graph
        if nodes.is_empty() || estimates <= 0.0 {
            continue;
        }
        let mut graph = algorithms::Projection::weighted();
        for node_arcs in &arcs {
            graph.weighted_node(node_arcs.iter().map(|&(to, w)| (to as u32, w)));
        }
        let seeds: Vec<(u32, f64)> = (0..nodes.len() as u32).map(|at| (at, 1.0)).collect();
        let truth = algorithms::rank(&graph, &seeds, alpha, 1_000_000, 1e-13).expect("a seed in a graph of its nodes");
        let distance: f64 = nodes.iter().zip(&truth.scores).map(|((_, n), s)| (n.x / estimates - s).abs()).sum();
        let bound = error_bound(residuals, estimates, nodes.len() as u64, alpha);
        if distance > bound + 1e-9 {
            out.push(violation(format!("its scores are {distance} from PageRank, past the bound {bound} it reports")));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_is_twice_the_residuals_over_the_kept_share_and_the_estimates() {
        let totals =
            Totals { nodes: 10, estimates: 9.0, residuals: 0.045, pushes: 0, stage: Stage::Built, swept: None };
        // 2 × 0.045 / (0.15 × 9)
        assert!((totals.error(0.85) - 0.0666_6666_6666_6667).abs() < 1e-12, "{}", totals.error(0.85));
        assert_eq!(Totals { nodes: 0, ..totals }.error(0.85), 0.0);
        assert_eq!(Totals { estimates: 0.0, ..totals }.error(0.85), f64::INFINITY, "nothing a read could divide by");
        assert_eq!(Totals { estimates: -1.0, ..totals }.error(0.85), f64::INFINITY);
    }

    #[test]
    fn a_read_rounds_scores_finer_than_the_bound_over_the_nodes_and_reports_a_bound_that_still_holds() {
        let (bound, step) = reported(3.2e-7, 1000);
        // 3.2e-7 over 1,000 nodes is 3.2e-10, so the step is 1e-10, and rounding adds at most 1,000 × 5e-11
        assert_eq!(step, 1e-10);
        assert!((3.2e-7 + 1000.0 * 1e-10 / 2.0..=4e-7).contains(&bound), "{bound}");
        assert_eq!(reported(0.0, 10), (0.0, 0.0));
        assert_eq!(reported(1e-3, 0), (0.0, 0.0));
        assert_eq!(reported(f64::INFINITY, 10), (f64::INFINITY, 0.0));
        assert_eq!(rounded(0.123456, 1e-3), 0.123);
        assert_eq!(rounded(0.1236, 1e-3), 0.124);
        assert_eq!(rounded(0.123456, 0.0), 0.123456);
    }

    #[test]
    fn the_score_order_keeps_sixteen_significant_bits() {
        let node = |x: f64| Node { x, r: 0.0, w: 0.0, type_at: 0, seq: 7 };
        // Apart in the 20th bit: the same key; apart in the 10th: another, and the higher estimate first
        assert_eq!(order_key("i", &node(1.0)), order_key("i", &node(1.0 + 2f64.powi(-20))));
        assert_ne!(order_key("i", &node(1.0)), order_key("i", &node(1.0 + 2f64.powi(-10))));
        assert!(order_key("i", &node(1.5)) < order_key("i", &node(1.0)));
    }

    #[test]
    fn two_states_have_the_same_order_exactly_when_their_keys_are_the_same() {
        let node = |x: f64, seq: u64| Node { x, r: 0.0, w: 0.0, type_at: 0, seq };
        let xs = [0.0, 1.0, 1.0 + 2f64.powi(-20), 1.0 + 2f64.powi(-10), 1.5, 3.0e-7, 123.456, 1e12];
        for &a in &xs {
            for &b in &xs {
                for (sa, sb) in [(7, 7), (7, 8)] {
                    let (p, q) = (node(a, sa), node(b, sb));
                    assert_eq!(same_order(&p, &q), order_key("i", &p) == order_key("i", &q), "{a} {sa}, {b} {sb}");
                }
            }
        }
    }
}
