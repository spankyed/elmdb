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
use std::rc::Rc;
use std::sync::Arc;

use crate::error::{Error, ErrorCode, Result};
use crate::schema::{RankDef, Schema};
use crate::sync::PlainMutex;

use super::derive::Violation;
use super::ids;
use super::keys;
use super::mirror::EdgeMirror;
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
    /// The changes to a node's estimate or residual since `estimates` and `residuals` were counted from the nodes: each
    /// adds its rounding to both, so `settle` counts them afresh once `Index::count_due` says, a full count's cost spread
    /// over the changes that called for it
    pub moved: u64,
    /// The store's sequence as the last commit to write the index read it, by `put_totals`: a drain computed outside the
    /// write lock is written only if this is what it read. Every commit raises the sequence and nothing lowers it, so a
    /// later commit can't leave the number this one did, an index cleared and rebuilt included
    pub written_at: u64,
    pub stage: Stage,
    /// Where the sweep of the queue is: the place of the node pushed last, which the next push starts after
    pub swept: Option<(u32, u64)>,
}

impl Totals {
    fn empty() -> Self {
        Totals { nodes: 0, estimates: 0.0, residuals: 0.0, moved: 0, written_at: 0, stage: Stage::Joining, swept: None }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(53);
        out.extend_from_slice(&self.nodes.to_le_bytes());
        out.extend_from_slice(&self.estimates.to_le_bytes());
        out.extend_from_slice(&self.residuals.to_le_bytes());
        out.extend_from_slice(&self.moved.to_le_bytes());
        out.extend_from_slice(&self.written_at.to_le_bytes());
        out.push(self.stage as u8);
        if let Some((type_at, seq)) = self.swept {
            out.extend_from_slice(&type_at.to_le_bytes());
            out.extend_from_slice(&seq.to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = || Error::new(ErrorCode::Corrupted, "A rank index's totals don't read");
        if bytes.len() != 41 && bytes.len() != 53 {
            return Err(corrupt());
        }
        let swept = (bytes.len() == 53).then(|| {
            (
                u32::from_le_bytes(bytes[41..45].try_into().expect("four bytes")),
                u64::from_le_bytes(bytes[45..53].try_into().expect("eight bytes")),
            )
        });
        let f = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
        let stage = match bytes[40] {
            0 => Stage::Joining,
            1 => Stage::Pushing,
            2 => Stage::Built,
            _ => return Err(corrupt()),
        };
        Ok(Totals {
            nodes: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            estimates: f(8),
            residuals: f(16),
            moved: u64::from_le_bytes(bytes[24..32].try_into().expect("eight bytes")),
            written_at: u64::from_le_bytes(bytes[32..40].try_into().expect("eight bytes")),
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

/// How many times the tolerance one cell of a rank index's score order spans, as a share of the scores in it. An estimate
/// moves in proportion to the tolerance, so this is about how many drains an estimate stays in its cell, at any tolerance
const CELL_TOLERANCES: f64 = 256.0;

/// The significant bits of an estimate a rank index's score order keeps: enough that a cell spans `CELL_TOLERANCES` times
/// the tolerance. A changed tolerance rebuilds the index, so no key outlives the bits it was written with
pub fn order_precision(tolerance: f64) -> u32 {
    (1.0 / (CELL_TOLERANCES * tolerance)).log2().ceil().clamp(4.0, 52.0) as u32
}

/// A node's place in its index's score order: its cell, then its own place. An edge changed moves nearly every estimate a
/// little, and a push that leaves an estimate in its cell leaves the order's entry as it is, so a cell's width is how much
/// of a drain's writes are the order's. The order is promised between scores further apart than twice the larger of the
/// tolerance and the bound an index reports. A cell can be wider than that only for a score over about `2 /
/// CELL_TOLERANCES` of the estimates' sum, which few nodes can have, and a read of the top reads through those
fn order_key(index: &str, precision: u32, node: &Node) -> Vec<u8> {
    keys::rank_order(index, f64::from_bits(order_bits(node.x, precision)), node.type_at, node.seq)
}

/// An estimate's bits as its order key keeps them: sign, exponent and `precision` bits of the fraction
fn order_bits(x: f64, precision: u32) -> u64 {
    x.to_bits() & !((1u64 << (52 - precision)) - 1)
}

/// Whether two estimates are in one cell of a score order kept to `precision`
pub fn same_cell(a: f64, b: f64, precision: u32) -> bool {
    order_bits(a, precision) == order_bits(b, precision)
}

/// Whether a cell of a score order kept to `precision` holding `x` is wider than `width`
pub fn cell_wider_than(x: f64, precision: u32, width: f64) -> bool {
    let low = order_bits(x, precision);
    (f64::from_bits(low + (1u64 << (52 - precision))) - f64::from_bits(low)).abs() > width
}

/// Whether two states of one node have the same place in the score order, read without building either key: most
/// pushes leave it, and a drain over a graph whose links are spread writes back nearly every node
fn same_order(a: &Node, b: &Node, precision: u32) -> bool {
    same_cell(a.x, b.x, precision) && (a.type_at, a.seq) == (b.type_at, b.seq)
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
    /// The significant bits of an estimate its score order keeps
    precision: u32,
}

impl<'a> Index<'a> {
    fn new(name: &'a str, def: &'a RankDef, schema: &'a Schema) -> Self {
        let keep = 1.0 - def.damping;
        Index {
            name,
            def,
            schema,
            threshold: def.tolerance * keep * keep / (2.0 + def.tolerance),
            precision: order_precision(def.tolerance),
        }
    }

    /// The residuals' sum at which the bound a read reports is the index's tolerance, over estimates that sum to
    /// `estimates`: pushes stop once the sum is within it, though single nodes are still over the threshold, since only
    /// the sum is what the bound is of
    fn budget(&self, estimates: f64) -> f64 {
        self.def.tolerance * (1.0 - self.def.damping) * estimates.max(0.0) / 2.0
    }

    /// Whether the totals are due to be counted from the nodes: once as many changes as nodes have added their rounding
    /// to them, or sooner when a tolerance is so tight their worst-case rounding would reach a tenth of the budget
    fn count_due(&self, totals: &Totals) -> bool {
        let keep = 1.0 - self.def.damping;
        let tight = self.def.tolerance * keep / (20.0 * f64::EPSILON);
        totals.moved > 0 && (totals.moved >= totals.nodes || totals.moved as f64 >= tight)
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
        self.totals.moved += 1;
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
        // The first change of the unit began from the totals as it found them
        let owed = self.held.residuals > self.index.budget(self.held.estimates);
        t.state.rank_owed_before.entry(name.to_owned()).or_insert(owed);
        let threshold = self.index.threshold;
        let precision = self.index.precision;
        for (id, (before, after)) in &self.nodes {
            if before == after {
                continue;
            }
            if let Some(b) = before {
                if after.is_none_or(|a| !same_order(&a, b, precision)) {
                    t.del(&order_key(name, precision, b))?;
                }
                if b.r.abs() > threshold && after.is_none_or(|a| a.r.abs() <= threshold) {
                    t.del(&keys::rank_queued(name, b.type_at, b.seq))?;
                }
            }
            match after {
                Some(a) => {
                    t.put(&keys::rank_node(name, id), &a.encode())?;
                    if before.is_none_or(|b| !same_order(a, &b, precision)) {
                        t.put(&order_key(name, precision, a), id.as_bytes())?;
                    }
                    if a.r.abs() > threshold && before.is_none_or(|b| b.r.abs() <= threshold) {
                        t.put(&keys::rank_queued(name, a.type_at, a.seq), id.as_bytes())?;
                    }
                }
                None => t.del(&keys::rank_node(name, id))?,
            }
        }
        put_totals(t, name, self.totals)
    }
}

/// Writes a rank index's totals, stamped with the commit writing them: the one way they are written, so every write of
/// the index tells a drain computed before it
fn put_totals(t: &mut Txn<'_, '_>, index: &str, mut totals: Totals) -> Result<()> {
    totals.written_at = super::mirror::stored_sequence(&*t)?;
    t.put(&keys::rank_totals(index), &totals.encode())
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
        after = Some(place);
    }
    change.totals.swept = after;
    let waiting = change.totals.residuals > index.budget(change.totals.estimates);
    change.finish(t)?;
    Ok(waiting)
}

/// Drains a rank index in memory, computed and written in the transaction it is given. Whether any still wait
pub fn drain(t: &mut Txn<'_, '_>, index: &str, limit: usize) -> Result<bool> {
    let mirror = t.state.mirror.clone();
    if let Some(mirror) = &mirror {
        mirror.lock().at(super::mirror::stored_sequence(&*t)?);
    }
    match plan(&*t, t.schema, index, limit, mirror.as_ref())? {
        Some(planned) => planned.write(t),
        None => Ok(false),
    }
}

/// What a drain would write, read from one snapshot of its index
pub struct Drained {
    index: String,
    /// The commit that last changed the index in what was read: the drain holds only while it is the stored one
    read_at: u64,
    reached: Reached,
    threshold: f64,
    precision: u32,
    totals: Totals,
    waiting: bool,
}

/// What became of a drain computed by `plan`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Written, and whether any still wait
    Written { waiting: bool },
    /// The index was written since the drain read it, so nothing was: a commit's corrections are computed from the
    /// estimates it read, and another drain's pushes from the residuals
    Stale,
}

impl Drained {
    /// Whether it read every node, for a test
    #[cfg(test)]
    pub(crate) fn read_every_node(&self) -> bool {
        self.reached.all
    }

    /// Writes the drain if the index is as it read it
    pub fn apply(&self, t: &mut Txn<'_, '_>) -> Result<Applied> {
        if totals(&*t, &self.index)?.written_at != self.read_at {
            return Ok(Applied::Stale);
        }
        Ok(Applied::Written { waiting: self.write(t)? })
    }

    /// Writes what was computed: the nodes that moved, their places in score order and in the queue, and the totals.
    /// Whether any still wait
    fn write(&self, t: &mut Txn<'_, '_>) -> Result<bool> {
        let Drained { index, reached, threshold, precision, totals, waiting, .. } = self;
        let (threshold, precision) = (*threshold, *precision);
        for (at, (old, new)) in reached.before.iter().zip(&reached.nodes).enumerate() {
            if old == new {
                continue;
            }
            let id = &reached.ids[at];
            t.put(&keys::rank_node(index, id), &new.encode())?;
            if !same_order(old, new, precision) {
                t.del(&order_key(index, precision, old))?;
                t.put(&order_key(index, precision, new), id.as_bytes())?;
            }
            let (was, is) = (old.r.abs() > threshold, new.r.abs() > threshold);
            if was && !is {
                t.del(&keys::rank_queued(index, old.type_at, old.seq))?;
            } else if is && !was {
                t.put(&keys::rank_queued(index, new.type_at, new.seq), id.as_bytes())?;
            }
        }
        put_totals(t, index, *totals)?;
        Ok(*waiting)
    }
}

/// Computes a drain from what `t` reads, writing nothing: the nodes waiting to be pushed, read from its queue, pushed in
/// the sweep `push` makes until none waits or `limit` pushes are made, each node and its arcs read once as the pushes
/// first reach it. A drain costs the nodes its pushes reach, not the graph: a change whose residual spreads to a few
/// hundred nodes reads those. `t` may be a snapshot older than the store, so the mirror keeps what it reads only while that
/// is still the store's, and is held only for one node's arcs, so commits never wait on a drain for it
pub fn plan(
    t: &impl Read,
    schema: &Schema,
    index: &str,
    limit: usize,
    mirror: Option<&Arc<PlainMutex<EdgeMirror>>>,
) -> Result<Option<Drained>> {
    let Some(def) = schema.ranks.get(index) else { return Ok(None) };
    let index = Index::new(index, def, schema);
    let held = totals(t, index.name)?;
    let sequence = mirror.map(|_| super::mirror::stored_sequence(t)).transpose()?;
    if let (Some(mirror), Some(sequence)) = (mirror, sequence) {
        mirror.lock().reader_at(sequence);
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
    let mut sweep =
        Sweep { residuals: held.residuals, estimates: held.estimates, after: held.swept, pushes: 0, moved: 0 };
    // One node at a time while few are read: the next waiting after the place pushed last, from the first again once past
    // the last
    while !reached.all && sweep.pushes < limit && sweep.residuals > index.budget(sweep.estimates) {
        if reached.at.len() as u64 > most {
            reached.read_all(t, &index)?;
            break;
        }
        use std::ops::Bound;
        let next = sweep
            .after
            .and_then(|place| reached.waiting.range((Bound::Excluded(place), Bound::Unbounded)).next())
            .or_else(|| reached.waiting.iter().next())
            .map(|(_, &u)| u);
        let Some(u) = next else { break };
        if reached.arcs[u].is_none() {
            let mut graph = mirror.map(|m| m.lock());
            let current = graph.as_deref_mut().zip(sequence);
            // Its far ends passing the most read one at a time: every node read, and it is their sweep's to push
            let Some(arcs) = reached.arcs(t, &index, current, u, most)? else {
                reached.read_all(t, &index)?;
                break;
            };
            reached.arcs[u] = Some(arcs);
        }
        let Reached { nodes, arcs, waiting, .. } = &mut reached;
        let list = arcs[u].as_deref().unwrap_or_default();
        sweep.push(nodes, list, u, alpha, threshold, |place, v, over| {
            if over {
                waiting.insert(place, v);
            } else {
                waiting.remove(&place);
            }
        });
        waiting.remove(&(nodes[u].type_at, nodes[u].seq));
    }
    if reached.all {
        reached.sweep_all(t, &index, mirror, sequence, &mut sweep, limit)?;
    }
    // Owed pushes: past the budget with a node to push, so a drain never waits on a sum no push can lower
    let waiting = sweep.residuals > index.budget(sweep.estimates)
        && if reached.all { reached.nodes.iter().any(|n| n.r.abs() > threshold) } else { !reached.waiting.is_empty() };
    let totals = Totals {
        estimates: sweep.estimates,
        residuals: sweep.residuals,
        moved: held.moved + sweep.moved,
        swept: sweep.after.or(held.swept),
        ..held
    };
    Ok(Some(Drained {
        index: index.name.to_owned(),
        read_at: held.written_at,
        reached,
        threshold,
        precision: index.precision,
        totals,
        waiting,
    }))
}

/// A drain's pushes so far
struct Sweep {
    residuals: f64,
    estimates: f64,
    /// The place pushed last
    after: Option<(u32, u64)>,
    pushes: usize,
    moved: u64,
}

impl Sweep {
    /// Pushes a node: its residual into its estimate, and `α` of it along its arcs by weight, telling `crossed` of each
    /// far end whose residual crossed the threshold, with its place and number and whether it is over it now
    #[inline(always)]
    fn push(
        &mut self,
        nodes: &mut [Node],
        arcs: &[(usize, f64)],
        u: usize,
        alpha: f64,
        threshold: f64,
        mut crossed: impl FnMut((u32, u64), usize, bool),
    ) {
        let residual = nodes[u].r;
        self.residuals -= residual.abs();
        if nodes[u].w > 0.0 {
            let share = alpha * residual / nodes[u].w;
            self.moved += arcs.len() as u64;
            for &(v, weight) in arcs {
                let node = &mut nodes[v];
                let held = node.r.abs();
                node.r += share * weight;
                let now = node.r.abs();
                self.residuals += now - held;
                if (now > threshold) != (held > threshold) {
                    crossed((node.type_at, node.seq), v, now > threshold);
                }
            }
        }
        nodes[u].x += residual;
        self.estimates += residual;
        nodes[u].r = 0.0;
        self.after = Some((nodes[u].type_at, nodes[u].seq));
        self.pushes += 1;
        self.moved += 1;
    }
}

/// The nodes a drain's pushes have reached, each read once: as read, as pushed, and its arcs once it is pushed
#[derive(Default)]
pub struct Reached {
    /// Each entity read, and its place here when it is a node
    at: rustc_hash::FxHashMap<Rc<str>, Option<usize>>,
    ids: Vec<Rc<str>>,
    before: Vec<Node>,
    nodes: Vec<Node>,
    /// A node's arcs to other nodes, `(node, weight)`, read the first time it is pushed
    arcs: Vec<Option<Vec<(usize, f64)>>>,
    /// The nodes reached whose residual is over the threshold, by their places: every node waiting, the queue having been
    /// read. Kept until every node is read, when the sweep compares each residual as it walks the nodes instead. A place
    /// is a number from the store's one sequence, however few nodes a drain reaches, so nothing here is sized by it
    waiting: BTreeMap<(u32, u64), usize>,
    /// Whether every node is read
    all: bool,
}

impl Reached {
    /// The entity's place here, read from the store the first time; `None` when it isn't a node
    fn node(&mut self, t: &impl Read, index: &Index<'_>, id: &str) -> Result<Option<usize>> {
        if let Some(&at) = self.at.get(id) {
            return Ok(at);
        }
        Ok(match node(t, index.name, id)? {
            Some(n) => Some(self.add(index, id.to_owned(), n)),
            None => {
                self.at.insert(id.into(), None);
                None
            }
        })
    }

    fn add(&mut self, index: &Index<'_>, id: String, n: Node) -> usize {
        let at = self.nodes.len();
        let id: Rc<str> = id.into();
        self.at.insert(Rc::clone(&id), Some(at));
        if !self.all && n.r.abs() > index.threshold {
            self.waiting.insert((n.type_at, n.seq), at);
        }
        self.ids.push(id);
        self.before.push(n);
        self.nodes.push(n);
        self.arcs.push(None);
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
            if self.at.contains_key(id.as_str()) {
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
        // Swept by number from here on, so no node is kept in the set of the waiting
        self.waiting = BTreeMap::new();
        // Nothing read before: added in place order, so numbered in it as they are added
        let fresh = self.nodes.is_empty();
        if fresh {
            read.sort_unstable_by_key(|(_, n)| (n.type_at, n.seq));
        }
        self.at.reserve(read.len());
        self.ids.reserve(read.len());
        self.before.reserve(read.len());
        self.nodes.reserve(read.len());
        self.arcs.reserve(read.len());
        for (id, n) in read {
            self.add(index, id, n);
        }
        if !fresh {
            self.in_place_order();
        }
        Ok(())
    }

    /// Every node numbered again in place order, the sweep's, so pushes sweeping it read the nodes and their arcs in
    /// the order they are stored here
    fn in_place_order(&mut self) {
        let mut order: Vec<usize> = (0..self.nodes.len()).collect();
        order.sort_unstable_by_key(|&at| (self.nodes[at].type_at, self.nodes[at].seq));
        let mut renumbered = vec![0; order.len()];
        for (to, &from) in order.iter().enumerate() {
            renumbered[from] = to;
        }
        let mut arcs = std::mem::take(&mut self.arcs);
        self.ids = order.iter().map(|&at| Rc::clone(&self.ids[at])).collect();
        self.before = order.iter().map(|&at| self.before[at]).collect();
        self.nodes = order.iter().map(|&at| self.nodes[at]).collect();
        self.arcs = order
            .iter()
            .map(|&at| arcs[at].take().map(|list| list.into_iter().map(|(v, w)| (renumbered[v], w)).collect()))
            .collect();
        for at in self.at.values_mut().flatten() {
            *at = renumbered[*at];
        }
    }

    /// The sweep once every node is read and numbered in place order, walking the numbers: a residual is compared where it
    /// is read, and no set of the waiting is kept up as pushes move them. Takes up after the place pushed last, and from
    /// the first again once past the last, until no node waits, `limit` pushes are made or the sum is within its budget
    fn sweep_all(
        &mut self,
        t: &impl Read,
        index: &Index<'_>,
        mirror: Option<&Arc<PlainMutex<EdgeMirror>>>,
        sequence: Option<u64>,
        sweep: &mut Sweep,
        limit: usize,
    ) -> Result<()> {
        let (threshold, alpha) = (index.threshold, index.def.damping);
        let n = self.nodes.len();
        let mut at = sweep.after.map_or(0, |place| self.nodes.partition_point(|n| (n.type_at, n.seq) <= place));
        let mut seen = 0;
        while sweep.pushes < limit && seen < n && sweep.residuals > index.budget(sweep.estimates) {
            if at >= n {
                at = 0;
            }
            if self.nodes[at].r.abs() <= threshold {
                at += 1;
                seen += 1;
                continue;
            }
            if self.arcs[at].is_none() {
                let mut graph = mirror.map(|m| m.lock());
                let current = graph.as_deref_mut().zip(sequence);
                let arcs = self.arcs_among_all(t, index, current, at)?;
                self.arcs[at] = Some(arcs);
            }
            let list = self.arcs[at].as_deref().unwrap_or_default();
            sweep.push(&mut self.nodes, list, at, alpha, threshold, |_, _, _| {});
            at += 1;
            seen = 0;
        }
        Ok(())
    }

    /// A node's arcs, as `Change::arcs_of` reads them, every node being read: a far end not here is no node. Apart from
    /// `arcs`, whose reads of a few nodes this would otherwise share a body with: together this costs nearly twice as much
    fn arcs_among_all(
        &self,
        t: &impl Read,
        index: &Index<'_>,
        mut graph: Option<(&mut super::mirror::EdgeMirror, u64)>,
        u: usize,
    ) -> Result<Vec<(usize, f64)>> {
        let mut arcs = Vec::new();
        for kind in &index.def.relations {
            for &outward in index.def.direction.sides() {
                let at = &self.at;
                let mut arc = |far: &str, weight: Option<f64>| {
                    if let Some(Some(v)) = at.get(far) {
                        arcs.push((*v, index.weight(kind, weight)));
                    }
                };
                match &mut graph {
                    Some((graph, at)) => graph.visit_far_ends(t, *at, &self.ids[u], outward, kind, arc)?,
                    None => relations::visit_far_ends(t, &self.ids[u], outward, Some(kind), |far, weight| {
                        arc(far, weight);
                        Ok(true)
                    })?,
                }
            }
        }
        Ok(arcs)
    }

    /// A node's arcs, as `Change::arcs_of` reads them: kind by kind, each side, in stored order, those to a node. `None`
    /// when reading its far ends one at a time would pass `most` entities read
    fn arcs(
        &mut self,
        t: &impl Read,
        index: &Index<'_>,
        mut graph: Option<(&mut super::mirror::EdgeMirror, u64)>,
        u: usize,
        most: u64,
    ) -> Result<Option<Vec<(usize, f64)>>> {
        // A far end already read is taken by its place here; one not read yet is named, read once the visit is done
        enum Far {
            Read(usize, f64),
            Unread(String, f64),
        }
        let id = Rc::clone(&self.ids[u]);
        let mut found: Vec<Far> = Vec::new();
        for kind in &index.def.relations {
            for &outward in index.def.direction.sides() {
                let at = &self.at;
                let mut arc = |far: &str, weight: Option<f64>| {
                    let weight = index.weight(kind, weight);
                    match at.get(far) {
                        Some(Some(v)) => found.push(Far::Read(*v, weight)),
                        Some(None) => {}
                        None => found.push(Far::Unread(far.to_owned(), weight)),
                    }
                };
                let visited = match &mut graph {
                    Some((graph, at)) => graph.visit_far_ends(t, *at, &id, outward, kind, arc),
                    None => relations::visit_far_ends(t, &id, outward, Some(kind), |far, weight| {
                        arc(far, weight);
                        Ok(true)
                    }),
                };
                visited?;
            }
        }
        let unread = found.iter().filter(|far| matches!(far, Far::Unread(..))).count();
        if (self.at.len() + unread) as u64 > most {
            return Ok(None);
        }
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
        Ok(Some(arcs))
    }
}

/// A commit's own pushes, up to `COMMIT_PUSHES` for each rank index it changed that owed none before it; the indexes it
/// leaves owing pushes or a count, for the drain. One that owed some has a drain coming that will do them over what it
/// pushed. A commit that didn't change an index writes nothing to it, so it never disturbs a drain being computed outside
/// the write lock. Whether a commit pushes follows from the history alone
pub fn push_at_commit(t: &mut Txn<'_, '_>) -> Result<Vec<String>> {
    let names: Vec<String> = t.schema.ranks.keys().cloned().collect();
    let sequence = super::mirror::stored_sequence(&*t)?;
    let mut waiting = Vec::new();
    for name in names {
        let def = &t.schema.ranks[&name];
        let held = totals(&*t, &name)?;
        // Every commit before this one stamped a lower sequence, so only this one's writes leave this number
        let changed = held.written_at == sequence;
        let owed = if changed && !t.state.rank_owed_before.get(&name).copied().unwrap_or(false) {
            push(t, &name, COMMIT_PUSHES)?
        } else {
            held.residuals > Index::new(&name, def, t.schema).budget(held.estimates)
        };
        if owed || Index::new(&name, def, t.schema).count_due(&totals(&*t, &name)?) {
            waiting.push(name);
        }
    }
    Ok(waiting)
}

/// Whether a rank index owes the builder anything: its build, pushes or a count of its totals
pub fn owes_work(r: &impl Read, index: &str) -> Result<bool> {
    let Some(def) = r.schema().ranks.get(index) else { return Ok(false) };
    let index = Index::new(index, def, r.schema());
    let totals = totals(r, index.name)?;
    Ok(totals.stage != Stage::Built || totals.residuals > index.budget(totals.estimates) || index.count_due(&totals))
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

/// After a drain owes no more pushes: the index built, and its totals counted again from the nodes when its first build
/// ends or `Index::count_due` says
pub fn settle(t: &mut Txn<'_, '_>, index: &str) -> Result<()> {
    let mut totals = totals(t, index)?;
    let def = t.schema.ranks.get(index);
    let due = def.is_none_or(|def| Index::new(index, def, t.schema).count_due(&totals));
    if totals.stage == Stage::Built && !due {
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
    totals.moved = 0;
    if totals.stage == Stage::Pushing {
        totals.stage = Stage::Built;
    }
    put_totals(t, index, totals)
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
            if n.x.is_finite() && !r.contains(&order_key(name, order_precision(def.tolerance), n))? {
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
        let totals = Totals {
            nodes: 10,
            estimates: 9.0,
            residuals: 0.045,
            moved: 0,
            written_at: 0,
            stage: Stage::Built,
            swept: None,
        };
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
    fn the_score_order_keeps_the_bits_its_tolerance_calls_for() {
        assert_eq!(order_precision(1e-6), 12);
        assert_eq!(order_precision(1e-4), 6);
        // Past where a fraction's bits end, and looser than a cell can usefully be
        assert_eq!(order_precision(1e-300), 52);
        assert_eq!(order_precision(0.5), 4);
    }

    #[test]
    fn a_cell_is_wider_than_a_width_by_the_gap_its_bits_leave_at_its_score() {
        // At 1 and 12 bits a cell spans 2^-12; at 2, twice that; a score's own place in it doesn't matter
        let span = 2f64.powi(-12);
        for x in [1.0, 1.0 + span / 2.0] {
            assert!(cell_wider_than(x, 12, span * 0.99), "{x}");
            assert!(!cell_wider_than(x, 12, span), "{x}");
        }
        assert!(cell_wider_than(2.0, 12, span * 1.99));
        assert!(!cell_wider_than(2.0, 12, span * 2.0));
        // No bit of the fraction left to widen past: the gap between two neighbouring numbers
        assert!(!cell_wider_than(1.0, 52, f64::EPSILON));
    }

    #[test]
    fn the_score_order_keeps_as_many_bits_as_it_is_given() {
        let node = |x: f64| Node { x, r: 0.0, w: 0.0, type_at: 0, seq: 7 };
        for precision in [4, 12, 16, 52] {
            let key = |x: f64| order_key("i", precision, &node(x));
            // Apart past the bits kept: one key. Apart in the last bit kept: another, and the higher estimate first
            if precision < 52 {
                assert_eq!(key(1.0), key(1.0 + 2f64.powi(-(precision as i32) - 1)), "{precision}");
            }
            assert!(key(1.0 + 2f64.powi(-(precision as i32))) < key(1.0), "{precision}");
        }
    }

    #[test]
    fn two_states_have_the_same_order_exactly_when_their_keys_are_the_same() {
        let node = |x: f64, seq: u64| Node { x, r: 0.0, w: 0.0, type_at: 0, seq };
        let xs = [0.0, 1.0, 1.0 + 2f64.powi(-20), 1.0 + 2f64.powi(-10), 1.5, 3.0e-7, 123.456, 1e12];
        for precision in [4, 12, 16, 52] {
            for &a in &xs {
                for &b in &xs {
                    for (sa, sb) in [(7, 7), (7, 8)] {
                        let (p, q) = (node(a, sa), node(b, sb));
                        assert_eq!(
                            same_order(&p, &q, precision),
                            order_key("i", precision, &p) == order_key("i", precision, &q),
                            "{a} {sa}, {b} {sb}, {precision}"
                        );
                    }
                }
            }
        }
    }
}
