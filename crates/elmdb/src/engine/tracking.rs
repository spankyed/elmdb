//! Tracked reads (`docs/architecture/engine.md`, Change sets and watch): what reads touched — each key they got, present
//! or not, and each range they scanned, as far as the scan went — filed under an owner in an index that finds, for the
//! keys a commit wrote, every owner whose reads they fall in: a map of keys, and an interval treap over the ranges, so
//! a written key costs one lookup and a stabbing query of O(log N + matches), N being the ranges filed.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::Hash;

use crate::error::Result;
use crate::format::key::prefix_range;
use crate::schema::Schema;
use crate::store::Db;

use super::unit::Read;

/// What reads touched: the keys they got, and the ranges `[low, high)` they scanned, `None` running to the store's end
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReadSet {
    pub keys: BTreeSet<Vec<u8>>,
    pub ranges: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// The least key above every key `key` starts: `key` with its trailing `0xFF`s dropped and its last byte raised, or the
/// store's end for one of nothing but `0xFF`s
fn above_all_starting(key: &[u8]) -> Option<Vec<u8>> {
    let mut high = key.to_vec();
    while high.last() == Some(&0xFF) {
        high.pop();
    }
    let last = high.last_mut()?;
    *last += 1;
    Some(high)
}

/// The least key above `key`
fn successor(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

/// A reader that records what it reads through another
pub struct Recording<'r, R> {
    inner: &'r R,
    reads: RefCell<ReadSet>,
}

impl<'r, R: Read> Recording<'r, R> {
    pub fn new(inner: &'r R) -> Self {
        Recording { inner, reads: RefCell::new(ReadSet::default()) }
    }

    pub fn into_reads(self) -> ReadSet {
        self.reads.into_inner()
    }

    /// Records a scan from `low` to `high`, or, when the visitor stopped it, to just past the last key it was shown
    fn scanned(&self, low: Vec<u8>, high: Option<Vec<u8>>, stopped_at: Option<Vec<u8>>) {
        let high = match stopped_at {
            Some(last) => Some(successor(&last)),
            None => high,
        };
        self.reads.borrow_mut().ranges.push((low, high));
    }
}

impl<R: Read> Read for Recording<'_, R> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.reads.borrow_mut().keys.insert(key.to_vec());
        self.inner.get(key)
    }

    fn visit(&self, prefix: &[u8], mut visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        let mut stopped_at = None;
        let outcome = self.inner.visit(prefix, |k, v| {
            let go = visit(k, v);
            if !go {
                stopped_at = Some(k.to_vec());
            }
            go
        });
        let (low, high) = prefix_range(prefix);
        // A scan that failed is recorded whole
        self.scanned(low, Some(high), if outcome.is_ok() { stopped_at } else { None });
        outcome
    }

    fn visit_starting(&self, bytes: &[u8], mut visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        let mut stopped_at = None;
        let outcome = self.inner.visit_starting(bytes, |k, v| {
            let go = visit(k, v);
            if !go {
                stopped_at = Some(k.to_vec());
            }
            go
        });
        self.scanned(bytes.to_vec(), above_all_starting(bytes), if outcome.is_ok() { stopped_at } else { None });
        outcome
    }

    fn schema(&self) -> &Schema {
        self.inner.schema()
    }

    fn raw(&self) -> (&heed::RoTxn<'_, heed::AnyTls>, Db) {
        self.inner.raw()
    }

    fn note(&self, prefix: &[u8]) {
        let (low, high) = prefix_range(prefix);
        self.reads.borrow_mut().ranges.push((low, Some(high)));
    }
}

/// Whether a range's end, `None` being the store's end, is above `key`
fn ends_above(end: &Option<Vec<u8>>, key: &[u8]) -> bool {
    end.as_deref().is_none_or(|end| end > key)
}

/// The higher of two ends
fn higher(a: &Option<Vec<u8>>, b: &Option<Vec<u8>>) -> Option<Vec<u8>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b).clone()),
        _ => None,
    }
}

/// A range in the treap, ordered by `(low, id)`; `max` is the highest end in its subtree, which lets a stabbing query
/// skip a subtree no range of which reaches the key
struct Node<W> {
    low: Vec<u8>,
    high: Option<Vec<u8>>,
    id: u64,
    owner: W,
    priority: u64,
    max: Option<Vec<u8>>,
    left: Option<Box<Node<W>>>,
    right: Option<Box<Node<W>>>,
}

type Tree<W> = Option<Box<Node<W>>>;

impl<W> Node<W> {
    fn refresh(mut self: Box<Self>) -> Box<Self> {
        let mut max = self.high.clone();
        for child in [&self.left, &self.right].into_iter().flatten() {
            max = higher(&max, &child.max);
        }
        self.max = max;
        self
    }

    fn below(&self, low: &[u8], id: u64) -> bool {
        (self.low.as_slice(), self.id) < (low, id)
    }
}

/// The nodes ordered below `(low, id)`, and the rest
fn split<W>(tree: Tree<W>, low: &[u8], id: u64) -> (Tree<W>, Tree<W>) {
    let Some(mut node) = tree else { return (None, None) };
    if node.below(low, id) {
        let (left, right) = split(node.right.take(), low, id);
        node.right = left;
        (Some(node.refresh()), right)
    } else {
        let (left, right) = split(node.left.take(), low, id);
        node.left = right;
        (left, Some(node.refresh()))
    }
}

/// Two treaps as one, every node of `a` ordered before every node of `b`
fn merge<W>(a: Tree<W>, b: Tree<W>) -> Tree<W> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(mut a), Some(mut b)) => {
            if a.priority > b.priority {
                a.right = merge(a.right.take(), Some(b));
                Some(a.refresh())
            } else {
                b.left = merge(Some(a), b.left.take());
                Some(b.refresh())
            }
        }
    }
}

/// Calls `found` with the owner of every range in `tree` holding `key`: `low <= key < high`
fn stab<W>(tree: &Tree<W>, key: &[u8], found: &mut impl FnMut(&W)) {
    let Some(node) = tree else { return };
    if !ends_above(&node.max, key) {
        return;
    }
    stab(&node.left, key, found);
    if node.low.as_slice() > key {
        return;
    }
    if ends_above(&node.high, key) {
        found(&node.owner);
    }
    stab(&node.right, key, found);
}

/// What an owner has filed, so it can be taken out again, and each range once: a range it reads again is filed already
#[derive(Default)]
struct Entry {
    keys: Vec<Vec<u8>>,
    nodes: Vec<(Vec<u8>, u64)>,
    ranges: HashSet<(Vec<u8>, Option<Vec<u8>>)>,
}

/// The read sets of every owner, indexed by what a write would have to touch to change what they read
pub struct TrackIndex<W> {
    keys: HashMap<Vec<u8>, HashSet<W>>,
    root: Tree<W>,
    ranges: usize,
    next_id: u64,
    /// The treap's priorities: a xorshift sequence, since a priority needs only to be spread, not secret
    seed: u64,
    entries: HashMap<W, Entry>,
}

impl<W> Default for TrackIndex<W> {
    fn default() -> Self {
        TrackIndex {
            keys: HashMap::new(),
            root: None,
            ranges: 0,
            next_id: 0,
            seed: 0x9E37_79B9_7F4A_7C15,
            entries: HashMap::new(),
        }
    }
}

impl<W: Clone + Eq + Hash + Ord> TrackIndex<W> {
    fn priority(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    /// Adds `reads` to what `owner` has filed
    pub fn add(&mut self, owner: &W, reads: ReadSet) {
        let mut entry = self.entries.remove(owner).unwrap_or_default();
        for key in reads.keys {
            if self.keys.entry(key.clone()).or_default().insert(owner.clone()) {
                entry.keys.push(key);
            }
        }
        for (low, high) in reads.ranges {
            if high.as_deref().is_some_and(|high| high <= low.as_slice())
                || !entry.ranges.insert((low.clone(), high.clone()))
            {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let priority = self.priority();
            let node = Box::new(Node {
                max: high.clone(),
                low: low.clone(),
                high,
                id,
                owner: owner.clone(),
                priority,
                left: None,
                right: None,
            });
            let (before, after) = split(self.root.take(), &low, id);
            self.root = merge(merge(before, Some(node)), after);
            entry.nodes.push((low, id));
            self.ranges += 1;
        }
        self.entries.insert(owner.clone(), entry);
    }

    /// Takes out everything `owner` has filed
    pub fn remove(&mut self, owner: &W) {
        let Some(entry) = self.entries.remove(owner) else { return };
        for key in entry.keys {
            if let Some(owners) = self.keys.get_mut(&key) {
                owners.remove(owner);
                if owners.is_empty() {
                    self.keys.remove(&key);
                }
            }
        }
        for (low, id) in entry.nodes {
            let (before, rest) = split(self.root.take(), &low, id);
            let (_, after) = split(rest, &low, id + 1);
            self.root = merge(before, after);
            self.ranges -= 1;
        }
    }

    /// Every owner whose reads hold one of `written`, each once
    pub fn touched_by<'k>(&self, written: impl IntoIterator<Item = &'k [u8]>) -> BTreeSet<W> {
        let mut touched = BTreeSet::new();
        for key in written {
            if let Some(owners) = self.keys.get(key) {
                touched.extend(owners.iter().cloned());
            }
            stab(&self.root, key, &mut |owner| {
                touched.insert(owner.clone());
            });
        }
        touched
    }

    /// Every owner that has filed reads
    pub fn owners(&self) -> BTreeSet<W> {
        self.entries.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many keys and ranges are filed
    pub fn size(&self) -> (usize, usize) {
        (self.keys.len(), self.ranges)
    }
}

#[cfg(test)]
mod tests;
