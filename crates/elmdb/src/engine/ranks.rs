//! Rank indexes (`docs/archive/roadmap/rank-index.md` once built, `docs/architecture/engine.md`, Rank indexes): PageRank
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
//! within `R / (1 − α)` of `x` in L1, where `R = Σ|r|`; since `x ≥ b`, the normalized scores are within
//! `E = 2R / ((1 − α)² n)` of PageRank, which a read reports. A node whose residual is over `ε = tolerance (1 − α)² / 2`
//! waits in the queue, so once none does, `E` is within the index's tolerance.
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
    pub stage: Stage,
    /// Where the sweep of the queue is: the place of the node pushed last, which the next push starts after
    pub swept: Option<(u32, u64)>,
}

impl Totals {
    fn empty() -> Self {
        Totals { nodes: 0, estimates: 0.0, residuals: 0.0, stage: Stage::Joining, swept: None }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(38);
        out.extend_from_slice(&self.nodes.to_le_bytes());
        out.extend_from_slice(&self.estimates.to_le_bytes());
        out.extend_from_slice(&self.residuals.to_le_bytes());
        out.push(self.stage as u8);
        if let Some((type_at, seq)) = self.swept {
            out.extend_from_slice(&type_at.to_le_bytes());
            out.extend_from_slice(&seq.to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = || Error::new(ErrorCode::Corrupted, "A rank index's totals don't read");
        if bytes.len() != 25 && bytes.len() != 37 {
            return Err(corrupt());
        }
        let swept = (bytes.len() == 37).then(|| {
            (
                u32::from_le_bytes(bytes[25..29].try_into().expect("four bytes")),
                u64::from_le_bytes(bytes[29..37].try_into().expect("eight bytes")),
            )
        });
        let f = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
        let stage = match bytes[24] {
            0 => Stage::Joining,
            1 => Stage::Pushing,
            2 => Stage::Built,
            _ => return Err(corrupt()),
        };
        Ok(Totals {
            nodes: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            estimates: f(8),
            residuals: f(16),
            stage,
            swept,
        })
    }

    /// The bound on the L1 distance of the normalized scores from PageRank, `E = 2R / ((1 − α)² n)`
    pub fn error(&self, damping: f64) -> f64 {
        if self.nodes == 0 {
            return 0.0;
        }
        let keep = 1.0 - damping;
        2.0 * self.residuals / (keep * keep * self.nodes as f64)
    }
}

/// A node's place in its index's score order: its estimate kept to 16 significant bits, so a push that moves the
/// estimate less than that leaves the order's entry as it is. An edge changed moves nearly every estimate a little;
/// rewriting each one's entry would be most of what a drain writes. The order is promised only between scores further
/// apart than twice the bound an index reports, far coarser than this, and a `ranked` read sorts by the estimates
/// themselves
fn order_key(index: &str, node: &Node) -> Vec<u8> {
    keys::rank_order(index, f64::from_bits(node.x.to_bits() & !((1u64 << 36) - 1)), node.type_at, node.seq)
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
    /// The residual over which a node waits to be pushed, `tolerance (1 − α)² / 2`
    threshold: f64,
}

impl<'a> Index<'a> {
    fn new(name: &'a str, def: &'a RankDef, schema: &'a Schema) -> Self {
        let keep = 1.0 - def.damping;
        Index { name, def, schema, threshold: def.tolerance * keep * keep / 2.0 }
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
                if after.is_none_or(|a| order_key(name, &a) != order_key(name, b)) {
                    t.del(&order_key(name, b))?;
                }
                if b.r.abs() > threshold && after.is_none_or(|a| a.r.abs() <= threshold) {
                    t.del(&keys::rank_queued(name, b.type_at, b.seq))?;
                }
            }
            match after {
                Some(a) => {
                    t.put(&keys::rank_node(name, id), &a.encode())?;
                    if before.is_none_or(|b| order_key(name, a) != order_key(name, &b)) {
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
        let next = match change.next_waiting(t, after)? {
            None if after.is_some() => change.next_waiting(t, None)?,
            next => next,
        };
        let Some((place, id)) = next else { break };
        change.push(t, &id)?;
        after = Some(place);
    }
    change.totals.swept = after;
    let waiting = !change.waiting.is_empty() || change.next_waiting(t, None)?.is_some();
    change.finish(t)?;
    Ok(waiting)
}

/// Drains a rank index in memory: its nodes and their arcs read once, pushed in the same sweep `push` makes until none
/// waits or `limit` pushes are made, and the nodes whose estimate or residual moved written back once. Reading the
/// graph costs what one `rank` read does; a push in memory then costs what an arc costs, where a push through the
/// store costs its reads and writes. Whether any still wait
pub fn drain(t: &mut Txn<'_, '_>, index: &str, limit: usize) -> Result<bool> {
    let schema = t.schema;
    let Some(def) = schema.ranks.get(index) else { return Ok(false) };
    let index = Index::new(index, def, schema);
    let held = totals(t, index.name)?;
    // The nodes in place order, which is the sweep's
    let mut ids: Vec<String> = Vec::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut failed = None;
    t.visit(&keys::rank_nodes(index.name), |key, bytes| {
        let id = match crate::format::key::decode(key).ok().and_then(|p| p.into_iter().nth(2)) {
            Some(crate::format::key::Part::String(id)) => id,
            _ => {
                failed = Some(Error::new(ErrorCode::Corrupted, "A rank index's node key doesn't read"));
                return false;
            }
        };
        match Node::decode(bytes) {
            Ok(n) => {
                ids.push(id);
                nodes.push(n);
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
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by_key(|&at| (nodes[at].type_at, nodes[at].seq));
    let ids: Vec<String> = order.iter().map(|&at| std::mem::take(&mut ids[at])).collect();
    let mut nodes: Vec<Node> = order.iter().map(|&at| nodes[at]).collect();
    let before = nodes.clone();
    let place: std::collections::HashMap<&str, usize> =
        ids.iter().enumerate().map(|(at, id)| (id.as_str(), at)).collect();
    // Each node's arcs, as `Change::arcs_of` reads them: kind by kind, each side, in stored order
    let mut arcs: Vec<Vec<(usize, f64)>> = Vec::with_capacity(ids.len());
    for id in &ids {
        let mut out = Vec::new();
        for kind in &def.relations {
            for &outward in def.direction.sides() {
                relations::visit_far_ends(t, id, outward, Some(kind), |far, weight| {
                    if let Some(&to) = place.get(far) {
                        out.push((to, index.weight(kind, weight)));
                    }
                    Ok(true)
                })?;
            }
        }
        arcs.push(out);
    }
    let threshold = index.threshold;
    let alpha = def.damping;
    let n = nodes.len();
    // The sweep starts after the place pushed last
    let mut at = match held.swept {
        Some(swept) => nodes.partition_point(|node| (node.type_at, node.seq) <= swept),
        None => 0,
    };
    let mut pushes = 0;
    let mut since_push = 0;
    while pushes < limit && n > 0 && since_push < n {
        if at == n {
            at = 0;
        }
        let u = at;
        at += 1;
        if nodes[u].r.abs() <= threshold {
            since_push += 1;
            continue;
        }
        since_push = 0;
        let residual = nodes[u].r;
        if nodes[u].w > 0.0 {
            let share = alpha * residual / nodes[u].w;
            for &(v, weight) in &arcs[u] {
                nodes[v].r += share * weight;
            }
        }
        nodes[u].x += residual;
        nodes[u].r = 0.0;
        pushes += 1;
    }
    let swept = (pushes > 0).then(|| {
        let last = if at == 0 { n - 1 } else { at - 1 };
        (nodes[last].type_at, nodes[last].seq)
    });
    let waiting = nodes.iter().any(|node| node.r.abs() > threshold);
    // Written back: the nodes that moved, their places in score order and in the queue, and the totals counted afresh
    for (at, (old, new)) in before.iter().zip(&nodes).enumerate() {
        if old == new {
            continue;
        }
        let id = &ids[at];
        t.put(&keys::rank_node(index.name, id), &new.encode())?;
        let (from, to) = (order_key(index.name, old), order_key(index.name, new));
        if from != to {
            t.del(&from)?;
            t.put(&to, id.as_bytes())?;
        }
        let (was, is) = (old.r.abs() > threshold, new.r.abs() > threshold);
        if was && !is {
            t.del(&keys::rank_queued(index.name, old.type_at, old.seq))?;
        } else if is && !was {
            t.put(&keys::rank_queued(index.name, new.type_at, new.seq), id.as_bytes())?;
        }
    }
    let totals = Totals {
        nodes: n as u64,
        estimates: nodes.iter().map(|node| node.x).sum(),
        residuals: nodes.iter().map(|node| node.r.abs()).sum(),
        stage: held.stage,
        swept: swept.or(held.swept),
    };
    t.put(&keys::rank_totals(index.name), &totals.encode())?;
    Ok(waiting)
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

/// After a drain empties the queue: the totals counted again from the nodes, which incremental sums drift from, and the
/// index built
pub fn settle(t: &mut Txn<'_, '_>, index: &str) -> Result<()> {
    let mut totals = totals(t, index)?;
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

/// The rank indexes not built yet, or with nodes waiting to be pushed, for the drain
pub fn unfinished(r: &impl Read) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for name in r.schema().ranks.keys() {
        let mut waiting = false;
        r.visit(&keys::rank_queue(name), |_, _| {
            waiting = true;
            false
        })?;
        if waiting || !is_built(r, name)? {
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
            if !r.contains(&order_key(name, n))? {
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
        let bound = Totals { nodes: nodes.len() as u64, estimates, residuals, ..totals }.error(alpha);
        if distance > bound + 1e-9 {
            out.push(violation(format!("its scores are {distance} from PageRank, past the bound {bound} it reports")));
        }
    }
    Ok(out)
}
