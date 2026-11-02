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

/// How many nodes one record holds: a node takes a slot as it joins, and its block is `slot / BLOCK`. A drain moves
/// nearly every node, so it writes a record per block rather than per node; a commit's pushes rewrite each block they
/// touch whole, so a block is kept small (`docs/evidence/rank-index.md`)
pub const BLOCK: u32 = 64;

/// A node's bytes in its block: whether the slot holds one, its estimate, residual and out-weight, its type's place
/// and its place
const ENTRY: usize = 37;

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
    fn write_entry(&self, out: &mut [u8]) {
        out[0] = 1;
        out[1..9].copy_from_slice(&self.x.to_le_bytes());
        out[9..17].copy_from_slice(&self.r.to_le_bytes());
        out[17..25].copy_from_slice(&self.w.to_le_bytes());
        out[25..29].copy_from_slice(&self.type_at.to_le_bytes());
        out[29..37].copy_from_slice(&self.seq.to_le_bytes());
    }

    /// The node an entry holds, `None` for an empty slot
    fn read_entry(bytes: &[u8]) -> Result<Option<Self>> {
        let f = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
        match bytes[0] {
            0 => Ok(None),
            1 => Ok(Some(Node {
                x: f(1),
                r: f(9),
                w: f(17),
                type_at: u32::from_le_bytes(bytes[25..29].try_into().expect("four bytes")),
                seq: u64::from_le_bytes(bytes[29..37].try_into().expect("eight bytes")),
            })),
            _ => Err(Error::new(ErrorCode::Corrupted, "A rank index's block holds an entry that doesn't read")),
        }
    }
}

/// A block's entries, one per slot
fn decode_block(bytes: &[u8]) -> Result<Vec<Option<Node>>> {
    if bytes.len() != BLOCK as usize * ENTRY {
        return Err(Error::new(ErrorCode::Corrupted, "A rank index's block doesn't read"));
    }
    bytes.as_chunks::<ENTRY>().0.iter().map(|entry| Node::read_entry(entry)).collect()
}

fn encode_block(entries: &[Option<Node>]) -> Vec<u8> {
    let mut out = vec![0; BLOCK as usize * ENTRY];
    for (entry, node) in out.as_chunks_mut::<ENTRY>().0.iter_mut().zip(entries) {
        if let Some(node) = node {
            node.write_entry(entry);
        }
    }
    out
}

/// The block a slot is in, and its place in it
fn block_of(slot: u32) -> (u32, usize) {
    (slot / BLOCK, (slot % BLOCK) as usize)
}

/// A block's entries as stored, every slot empty for a block that isn't
fn read_block(r: &impl Read, index: &str, block: u32) -> Result<Vec<Option<Node>>> {
    match r.get(&keys::rank_block(index, block))? {
        Some(bytes) => decode_block(&bytes),
        None => Ok(vec![None; BLOCK as usize]),
    }
}

/// Writes a block's entries, deleting a block no slot of which holds a node
fn write_block(t: &mut Txn<'_, '_>, index: &str, block: u32, entries: &[Option<Node>]) -> Result<()> {
    if entries.iter().all(Option::is_none) {
        t.del(&keys::rank_block(index, block))
    } else {
        t.put(&keys::rank_block(index, block), &encode_block(entries))
    }
}

/// The slot an entity's node is in, when it is one
fn slot_of(r: &impl Read, index: &str, entity: &str) -> Result<Option<u32>> {
    r.get(&keys::rank_slot(index, entity))?
        .map(|bytes| {
            bytes
                .as_slice()
                .try_into()
                .map(u32::from_le_bytes)
                .map_err(|_| Error::new(ErrorCode::Corrupted, "A rank index's slot doesn't read"))
        })
        .transpose()
}

/// A slot for a node joining: the lowest one a node left, or the next never taken
fn take_slot(t: &mut Txn<'_, '_>, index: &str, next: &mut u32, entity: &str) -> Result<u32> {
    let mut free = None;
    t.visit(&keys::rank_frees(index), |key, _| {
        free = number_part(key, 2).map(|n| n as u32);
        false
    })?;
    let slot = match free {
        Some(slot) => {
            t.del(&keys::rank_free(index, slot))?;
            slot
        }
        None => {
            let slot = *next;
            *next += 1;
            slot
        }
    };
    t.put(&keys::rank_slot(index, entity), &slot.to_le_bytes())?;
    t.put(&keys::rank_entity(index, slot), entity.as_bytes())?;
    Ok(slot)
}

/// A key's part at `at` as a number
fn number_part(key: &[u8], at: usize) -> Option<f64> {
    match crate::format::key::decode(key).ok()?.into_iter().nth(at)? {
        crate::format::key::Part::Number(n) => Some(n),
        _ => None,
    }
}

/// A rank index's node as stored: its slot, its entity and the node
pub struct Stored {
    pub slot: u32,
    pub id: String,
    pub node: Node,
}

/// A rank index's nodes as stored, in slot order, each slot's entity read from the slot's own record; what doesn't agree
/// is `problem`'s to say, and the node is left out
fn stored_with(r: &impl Read, index: &str, mut problem: impl FnMut(String)) -> Result<Vec<Stored>> {
    let mut entities: BTreeMap<u32, String> = BTreeMap::new();
    r.visit(&keys::rank_entities(index), |key, id| {
        match number_part(key, 2) {
            Some(slot) => {
                entities.insert(slot as u32, String::from_utf8_lossy(id).into_owned());
            }
            None => problem("holds a slot's entity whose key doesn't read".to_owned()),
        }
        true
    })?;
    let mut out = Vec::new();
    let mut failed = None;
    r.visit(&keys::rank_blocks(index), |key, bytes| {
        let Some(block) = number_part(key, 2).map(|n| n as u32) else {
            problem("holds a block whose key doesn't read".to_owned());
            return true;
        };
        let entries = match decode_block(bytes) {
            Ok(entries) => entries,
            Err(e) => {
                failed = Some(e);
                return false;
            }
        };
        for (at, entry) in entries.into_iter().enumerate() {
            let slot = block * BLOCK + at as u32;
            match (entry, entities.remove(&slot)) {
                (Some(node), Some(id)) => out.push(Stored { slot, id, node }),
                (Some(_), None) => problem(format!("holds a node in slot {slot}, which names no entity")),
                (None, Some(id)) => problem(format!("names {id} in slot {slot}, which holds no node")),
                (None, None) => {}
            }
        }
        true
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    for (slot, id) in entities {
        problem(format!("names {id} in slot {slot}, whose block isn't stored"));
    }
    Ok(out)
}

/// A rank index's nodes as stored, in slot order; an error for slots and entities that don't agree
pub fn stored(r: &impl Read, index: &str) -> Result<Vec<Stored>> {
    let mut first = None;
    let nodes = stored_with(r, index, |message| {
        first.get_or_insert(message);
    })?;
    match first {
        Some(message) => Err(Error::new(ErrorCode::Corrupted, format!("A rank index {message}"))),
        None => Ok(nodes),
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
    /// The next slot no node has taken
    pub next_slot: u32,
    /// Where the sweep of the queue is: the place of the node pushed last, which the next push starts after
    pub swept: Option<(u32, u64)>,
}

impl Totals {
    fn empty() -> Self {
        Totals { nodes: 0, estimates: 0.0, residuals: 0.0, stage: Stage::Joining, next_slot: 0, swept: None }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(41);
        out.extend_from_slice(&self.nodes.to_le_bytes());
        out.extend_from_slice(&self.estimates.to_le_bytes());
        out.extend_from_slice(&self.residuals.to_le_bytes());
        out.push(self.stage as u8);
        out.extend_from_slice(&self.next_slot.to_le_bytes());
        if let Some((type_at, seq)) = self.swept {
            out.extend_from_slice(&type_at.to_le_bytes());
            out.extend_from_slice(&seq.to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = || Error::new(ErrorCode::Corrupted, "A rank index's totals don't read");
        if bytes.len() != 29 && bytes.len() != 41 {
            return Err(corrupt());
        }
        let swept = (bytes.len() == 41).then(|| {
            (
                u32::from_le_bytes(bytes[29..33].try_into().expect("four bytes")),
                u64::from_le_bytes(bytes[33..41].try_into().expect("eight bytes")),
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
            next_slot: u32::from_le_bytes(bytes[25..29].try_into().expect("four bytes")),
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
    let Some(slot) = slot_of(r, index, entity)? else { return Ok(None) };
    let (block, at) = block_of(slot);
    let missing = || Error::new(ErrorCode::Corrupted, format!("A rank index's slot for {entity} holds no node"));
    let bytes = r.get(&keys::rank_block(index, block))?.ok_or_else(missing)?;
    if bytes.len() != BLOCK as usize * ENTRY {
        return Err(Error::new(ErrorCode::Corrupted, "A rank index's block doesn't read"));
    }
    Node::read_entry(&bytes[at * ENTRY..(at + 1) * ENTRY])?.ok_or_else(missing).map(Some)
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

    /// Writes what changed: each node into its block, a block once however many of its nodes changed, the slots of
    /// nodes joining and leaving, each node's place in score order and in the queue, and the totals
    fn finish(self, t: &mut Txn<'_, '_>) -> Result<()> {
        if self.totals == self.held && self.nodes.values().all(|(before, after)| before == after) {
            return Ok(());
        }
        let Change { index, mut totals, nodes, .. } = self;
        let name = index.name;
        let threshold = index.threshold;
        let mut blocks: BTreeMap<u32, Vec<Option<Node>>> = BTreeMap::new();
        for (id, (before, after)) in &nodes {
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
            if let Some(a) = after {
                if before.is_none_or(|b| !same_order(a, &b)) {
                    t.put(&order_key(name, a), id.as_bytes())?;
                }
                if a.r.abs() > threshold && before.is_none_or(|b| b.r.abs() <= threshold) {
                    t.put(&keys::rank_queued(name, a.type_at, a.seq), id.as_bytes())?;
                }
            }
            let slot = if before.is_none() {
                take_slot(t, name, &mut totals.next_slot, id)?
            } else {
                let slot = slot_of(t, name, id)?
                    .ok_or_else(|| Error::new(ErrorCode::Corrupted, format!("A rank index's node {id} has no slot")))?;
                if after.is_none() {
                    t.del(&keys::rank_slot(name, id))?;
                    t.del(&keys::rank_entity(name, slot))?;
                    t.put(&keys::rank_free(name, slot), &[])?;
                }
                slot
            };
            let (block, at) = block_of(slot);
            let entries = match blocks.entry(block) {
                std::collections::btree_map::Entry::Occupied(held) => held.into_mut(),
                std::collections::btree_map::Entry::Vacant(first) => first.insert(read_block(t, name, block)?),
            };
            entries[at] = *after;
        }
        for (block, entries) in &blocks {
            write_block(t, name, *block, entries)?;
        }
        t.put(&keys::rank_totals(name), &totals.encode())
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
        after = Some(place);
    }
    change.totals.swept = after;
    let waiting = change.totals.residuals > index.budget(change.totals.estimates);
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
    let mut held_nodes = stored(t, index.name)?;
    held_nodes.sort_by_key(|stored| (stored.node.type_at, stored.node.seq));
    let slots: Vec<u32> = held_nodes.iter().map(|stored| stored.slot).collect();
    let mut nodes: Vec<Node> = held_nodes.iter().map(|stored| stored.node).collect();
    let ids: Vec<String> = held_nodes.into_iter().map(|stored| stored.id).collect();
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
    let mut residuals: f64 = nodes.iter().map(|node| node.r.abs()).sum();
    // Each push adds its residual to the estimates, which the budget grows with
    let mut estimates: f64 = nodes.iter().map(|node| node.x).sum();
    let mut pushes = 0;
    let mut since_push = 0;
    while pushes < limit && n > 0 && since_push < n && residuals > index.budget(estimates) {
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
        residuals -= residual.abs();
        if nodes[u].w > 0.0 {
            let share = alpha * residual / nodes[u].w;
            for &(v, weight) in &arcs[u] {
                let held = nodes[v].r.abs();
                nodes[v].r += share * weight;
                residuals += nodes[v].r.abs() - held;
            }
        }
        nodes[u].x += residual;
        estimates += residual;
        nodes[u].r = 0.0;
        pushes += 1;
    }
    // A push advances the sweep past the node it took, so after any push `at` is at least 1
    let swept = (pushes > 0).then(|| (nodes[at - 1].type_at, nodes[at - 1].seq));
    // Owed pushes: past the budget with a node to push, so a drain never waits on a sum no push can lower
    let held_estimates: f64 = nodes.iter().map(|node| node.x).sum();
    let waiting = nodes.iter().map(|node| node.r.abs()).sum::<f64>() > index.budget(held_estimates)
        && nodes.iter().any(|node| node.r.abs() > threshold);
    // Written back: the blocks of the nodes that moved, their places in score order and in the queue, and the totals
    // counted afresh
    let mut moved = std::collections::BTreeSet::new();
    for (at, (old, new)) in before.iter().zip(&nodes).enumerate() {
        if old == new {
            continue;
        }
        let id = &ids[at];
        moved.insert(block_of(slots[at]).0);
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
    if !moved.is_empty() {
        let by_slot: std::collections::HashMap<u32, usize> =
            slots.iter().enumerate().map(|(at, slot)| (*slot, at)).collect();
        for block in moved {
            let entries: Vec<Option<Node>> =
                (block * BLOCK..(block + 1) * BLOCK).map(|slot| by_slot.get(&slot).map(|&at| nodes[at])).collect();
            write_block(t, index.name, block, &entries)?;
        }
    }
    let totals = Totals {
        nodes: n as u64,
        estimates: nodes.iter().map(|node| node.x).sum(),
        residuals: nodes.iter().map(|node| node.r.abs()).sum(),
        stage: held.stage,
        next_slot: held.next_slot,
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
    for Stored { node, .. } in stored(t, index)? {
        nodes += 1;
        estimates += node.x;
        residuals += node.r.abs();
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
    for prefix in keys::rank_data(index) {
        t.clear_prefix(&prefix)?;
    }
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
        let held = stored_with(r, name, |message| out.push(violation(message)))?;
        // Each slot's node and entity name each other, and a free slot holds neither and was taken once
        for Stored { slot, id, .. } in &held {
            if slot_of(r, name, id)? != Some(*slot) {
                out.push(violation(format!("holds {id} in slot {slot}, which its slot record doesn't name")));
            }
            if *slot >= totals.next_slot {
                out.push(violation(format!("holds {id} in slot {slot}, past the next slot {}", totals.next_slot)));
            }
        }
        let mut named = 0usize;
        r.visit(&keys::rank_slots(name), |_, _| {
            named += 1;
            true
        })?;
        if named != held.len() {
            out.push(violation(format!("names {named} slots for {} nodes", held.len())));
        }
        let taken: std::collections::BTreeSet<u32> = held.iter().map(|stored| stored.slot).collect();
        let mut frees = Vec::new();
        r.visit(&keys::rank_frees(name), |key, _| {
            frees.push(number_part(key, 2).map(|n| n as u32));
            true
        })?;
        for free in frees {
            match free {
                Some(slot) if taken.contains(&slot) || slot >= totals.next_slot => {
                    out.push(violation(format!("lists slot {slot} as free, which is taken or never was")))
                }
                Some(_) => {}
                None => out.push(violation("lists a free slot whose key doesn't read".to_owned())),
            }
        }
        let nodes: Vec<(String, Node)> = held.into_iter().map(|stored| (stored.id, stored.node)).collect();
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

/// A stored node changed in place, as damage to it: its slot's block read, the entry changed and written back
#[cfg(test)]
pub(crate) fn edit_stored(
    txn: &mut heed::RwTxn<'_>,
    d: crate::store::Db,
    index: &str,
    entity: &str,
    f: impl FnOnce(&mut Node),
) -> Result<()> {
    let slot = u32::from_le_bytes(d.main.get(txn, &keys::rank_slot(index, entity))?.unwrap().try_into().unwrap());
    let (block, at) = block_of(slot);
    let key = keys::rank_block(index, block);
    let mut entries = decode_block(d.main.get(txn, &key)?.unwrap())?;
    f(entries[at].as_mut().expect("a node in its slot"));
    d.main.put(txn, &key, &encode_block(&entries))?;
    Ok(())
}

/// A stored node, read as `edit_stored` finds it
#[cfg(test)]
pub(crate) fn read_stored(txn: &heed::RoTxn<'_>, d: crate::store::Db, index: &str, entity: &str) -> Result<Node> {
    let slot = u32::from_le_bytes(d.main.get(txn, &keys::rank_slot(index, entity))?.unwrap().try_into().unwrap());
    let (block, at) = block_of(slot);
    Ok(decode_block(d.main.get(txn, &keys::rank_block(index, block))?.unwrap())?[at].expect("a node in its slot"))
}

/// A node written into `slot` for `entity`, its slot named both ways, as damage: nothing else is told
#[cfg(test)]
pub(crate) fn put_stored(
    txn: &mut heed::RwTxn<'_>,
    d: crate::store::Db,
    index: &str,
    entity: &str,
    slot: u32,
    node: Node,
) -> Result<()> {
    let (block, at) = block_of(slot);
    let key = keys::rank_block(index, block);
    let mut entries = match d.main.get(txn, &key)? {
        Some(bytes) => decode_block(bytes)?,
        None => vec![None; BLOCK as usize],
    };
    entries[at] = Some(node);
    d.main.put(txn, &key, &encode_block(&entries))?;
    d.main.put(txn, &keys::rank_slot(index, entity), &slot.to_le_bytes())?;
    d.main.put(txn, &keys::rank_entity(index, slot), entity.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_is_twice_the_residuals_over_the_kept_share_and_the_estimates() {
        let totals =
            Totals { nodes: 10, estimates: 9.0, residuals: 0.045, stage: Stage::Built, next_slot: 10, swept: None };
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
