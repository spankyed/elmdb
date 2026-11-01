//! What a rank index's drain reads of the graph, kept between drains: each entity's edge entries out and in of a kind
//! (`d/o`, `d/n`, `docs/architecture/format.md`), in place order, as the store holds them at one commit.
//!
//! A drain reads every node's edges of its index's kinds, the most of what it costs (`docs/evidence/rank-index.md`),
//! though a commit between two drains moves a few. The mirror holds an entity's entries once a drain has read them, and
//! every commit made through the engine hands the store the entries it filed and unfiled (`EdgeDelta`), which the store
//! applies under its write lock as it commits, so a list the mirror holds is the store's at the mirror's sequence:
//!
//! - **A list held is complete**: read whole from the store at the mirror's sequence, and changed since only by the
//!   entries each commit filed or unfiled. An entity the mirror holds no list for is read from the store, as before.
//! - **A commit that can't say what it filed empties it**: a write outside the engine's units, an admin unit, a schema
//!   change; and so does a commit whose base isn't the mirror's sequence, another process's commit between them.
//! - **A drain asks the store's sequence in its own transaction** and empties the mirror when it differs, so another
//!   process's writes are read afresh.
//!
//! `check()` compares every list held with the store's entries.

use rustc_hash::FxHashMap;

use crate::error::Result;

use super::relations;
use super::unit::Read;

/// An edge entry filed or unfiled, out at the source and in at the target, as a commit hands it to the store
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeDelta {
    /// Filed at `seq`, or filed again there with a new weight
    Filed {
        kind: String,
        source: String,
        target: String,
        seq: u64,
        weight: Option<f64>,
    },
    Unfiled {
        kind: String,
        source: String,
        target: String,
        seq: u64,
    },
}

/// One entry of a list: its place, its far end's name and its weight, `NO_WEIGHT` when it holds none
#[derive(Debug, Clone, Copy, PartialEq)]
struct Entry {
    seq: u64,
    far: u32,
    weight: f64,
}

/// The weight an entry holding none keeps: a weight is from 0 to `relations::MAX_WEIGHT`, so no edge holds this one,
/// and an entry is 24 bytes where an `Option<f64>` would make it 32
const NO_WEIGHT: f64 = -1.0;

impl Entry {
    fn new(seq: u64, far: u32, weight: Option<f64>) -> Self {
        Entry { seq, far, weight: weight.unwrap_or(NO_WEIGHT) }
    }

    fn weight(&self) -> Option<f64> {
        (self.weight != NO_WEIGHT).then_some(self.weight)
    }
}

/// The edge entries held, by entity, kind and side, at one commit's sequence
#[derive(Debug, Default)]
pub struct EdgeMirror {
    /// The commit the lists are the store's at; `None` holds nothing
    sequence: Option<u64>,
    /// Every entity and kind name the lists mention, each once
    names: FxHashMap<Box<str>, u32>,
    ids: Vec<Box<str>>,
    lists: FxHashMap<(u32, u32, bool), Vec<Entry>>,
    /// How many entries the lists hold
    entries: usize,
    /// How many entries commits have unfiled or replaced since the names were last counted afresh: only those can leave
    /// a name no list mentions, at most two each
    removed: usize,
}

/// The entries removed however few the lists hold before the names are counted afresh, so a small mirror isn't counted
/// every few commits
const REMOVED_KEPT: usize = 1024;

impl EdgeMirror {
    fn name(&mut self, name: &str) -> u32 {
        if let Some(&at) = self.names.get(name) {
            return at;
        }
        let at = u32::try_from(self.ids.len()).expect("fewer than 2^32 names");
        self.ids.push(name.into());
        self.names.insert(name.into(), at);
        at
    }

    fn known(&self, name: &str) -> Option<u32> {
        self.names.get(name).copied()
    }

    /// Holds nothing, and no sequence
    pub fn clear(&mut self) {
        *self = EdgeMirror::default();
    }

    /// The mirror for a reader at `sequence`, the store's in the reader's transaction: emptied first if the lists are
    /// another commit's
    pub fn at(&mut self, sequence: u64) {
        if self.sequence != Some(sequence) {
            self.clear();
            self.sequence = Some(sequence);
        }
    }

    /// The commit the lists are the store's at, if any
    pub fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    /// A commit's entries applied: the lists held move from `base`, the sequence the commit read, to `next`, the one it
    /// wrote. Entries `None`, which a commit outside the engine's units hands, or a base the lists aren't at, empty it
    pub fn commit(&mut self, base: u64, next: u64, entries: Option<&[EdgeDelta]>) {
        let Some(entries) = entries.filter(|_| self.sequence == Some(base)) else {
            self.clear();
            return;
        };
        for delta in entries {
            match delta {
                EdgeDelta::Filed { kind, source, target, seq, weight } => {
                    self.file(source, kind, true, target, *seq, *weight);
                    self.file(target, kind, false, source, *seq, *weight);
                }
                EdgeDelta::Unfiled { kind, source, target, seq } => {
                    self.unfile(source, kind, true, *seq);
                    self.unfile(target, kind, false, *seq);
                }
            }
        }
        self.sequence = Some(next);
        // A name stays once a list mentioned it: the entities deleted and the far ends unfiled leave theirs. Counting
        // afresh once more entries were removed than the lists hold keeps the names no list mentions within twice the
        // entries held, and costs each removal a constant share; a drain filling lists removes none, so never counts
        if self.removed > self.entries.max(REMOVED_KEPT) {
            self.compact();
        }
    }

    /// The names kept again from only those the lists held mention, the lists unchanged but for their numbering
    fn compact(&mut self) {
        let ids = std::mem::take(&mut self.ids);
        let lists = std::mem::take(&mut self.lists);
        self.names.clear();
        for ((entity, kind, outward), list) in lists {
            let key = (self.name(&ids[entity as usize]), self.name(&ids[kind as usize]), outward);
            let list = list.into_iter().map(|e| Entry { far: self.name(&ids[e.far as usize]), ..e }).collect();
            self.lists.insert(key, list);
        }
        self.removed = 0;
    }

    fn list(&mut self, entity: &str, kind: &str, outward: bool) -> Option<&mut Vec<Entry>> {
        let key = (self.known(entity)?, self.known(kind)?, outward);
        self.lists.get_mut(&key)
    }

    /// An entry filed in a list held, or filed again at its place with another weight; a list not held stays unread
    fn file(&mut self, entity: &str, kind: &str, outward: bool, far: &str, seq: u64, weight: Option<f64>) {
        if self.list(entity, kind, outward).is_none() {
            return;
        }
        let far = self.name(far);
        let list = self.list(entity, kind, outward).expect("held above");
        match list.binary_search_by_key(&seq, |e| e.seq) {
            Ok(at) => {
                list[at] = Entry::new(seq, far, weight);
                self.removed += 1;
            }
            Err(at) => {
                list.insert(at, Entry::new(seq, far, weight));
                self.entries += 1;
            }
        }
    }

    /// An entry unfiled from a list held; a list it empties is dropped, so an entity deleted holds none, and one
    /// still a node reads its list from the store again
    fn unfile(&mut self, entity: &str, kind: &str, outward: bool, seq: u64) {
        let (Some(entity), Some(kind)) = (self.known(entity), self.known(kind)) else { return };
        let key = (entity, kind, outward);
        let Some(list) = self.lists.get_mut(&key) else { return };
        if let Ok(at) = list.binary_search_by_key(&seq, |e| e.seq) {
            list.remove(at);
            if list.is_empty() {
                self.lists.remove(&key);
            }
            self.entries -= 1;
            self.removed += 1;
        }
    }

    /// Visits the entity's entries of `kind` out (or in), in place order, as `relations::visit_far_ends` does: from the
    /// list held, or from the store, the list then held. Every entry is visited
    pub fn visit_far_ends(
        &mut self,
        t: &impl Read,
        entity: &str,
        outward: bool,
        kind: &str,
        mut each: impl FnMut(&str, Option<f64>),
    ) -> Result<()> {
        let key = (self.name(entity), self.name(kind), outward);
        if !self.lists.contains_key(&key) {
            // Filled straight from the store, and fitted: a list collected from a buffer of what was read would keep
            // that buffer, twice an entry's size
            let mut list = Vec::new();
            relations::visit_placed_far_ends(t, entity, outward, kind, |seq, far, weight| {
                list.push(Entry::new(seq, self.name(far), weight));
                Ok(true)
            })?;
            list.shrink_to_fit();
            self.entries += list.len();
            self.lists.insert(key, list);
        }
        for entry in &self.lists[&key] {
            each(&self.ids[entry.far as usize], entry.weight());
        }
        Ok(())
    }

    /// Each list held that differs from the store's entries at the reader's state, named: what `check()` reports, the
    /// reader at the mirror's sequence
    pub fn differences(&self, t: &impl Read) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for (&(entity, kind, outward), held) in &self.lists {
            let (entity, kind) = (&*self.ids[entity as usize], &*self.ids[kind as usize]);
            let mut stored = Vec::new();
            relations::visit_placed_far_ends(t, entity, outward, kind, |seq, far, weight| {
                stored.push((seq, far.to_owned(), weight));
                Ok(true)
            })?;
            let mirrored: Vec<_> =
                held.iter().map(|e| (e.seq, self.ids[e.far as usize].to_string(), e.weight())).collect();
            if mirrored != stored {
                let side = if outward { "out" } else { "in" };
                out.push(format!("the edge mirror holds {entity}'s {kind} edges {side} unlike the store"));
            }
        }
        out.sort();
        Ok(out)
    }

    /// How many lists are held, for tests
    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.lists.len()
    }

    /// Drops one entry from a list held, as a missed delta would, for a test of `check()`
    #[cfg(test)]
    pub fn lose_an_entry(&mut self) {
        let list = self.lists.values_mut().find(|l| !l.is_empty()).expect("a list holding an entry");
        list.pop();
        self.entries -= 1;
    }
}

/// The key a drain's mirror is read at: the stored sequence of the reader's transaction, which every commit raises
pub fn stored_sequence(t: &impl Read) -> Result<u64> {
    let bytes = t.get(&crate::store::meta_key(crate::format::meta::SEQUENCE))?;
    Ok(bytes.and_then(|b| <[u8; 8]>::try_from(b.as_slice()).ok()).map_or(0, u64::from_be_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filed(seq: u64) -> EdgeDelta {
        EdgeDelta::Filed { kind: "k".into(), source: "A".into(), target: "B".into(), seq, weight: None }
    }

    /// A mirror at sequence 1 holding A's `k` entries out, which are none
    fn mirror() -> EdgeMirror {
        let mut m = EdgeMirror::default();
        m.at(1);
        let key = (m.name("A"), m.name("k"), true);
        m.lists.insert(key, Vec::new());
        m
    }

    #[test]
    fn a_commit_from_the_mirrors_sequence_moves_it_and_its_lists_and_any_other_empties_it() {
        let mut m = mirror();
        m.commit(1, 2, Some(&[filed(5)]));
        assert_eq!((m.sequence(), m.held()), (Some(2), 1));
        assert_eq!(m.lists.values().next().unwrap().len(), 1, "A's list gains the entry; B's, not held, stays unread");
        // Filed again at its place, a new weight: the same entry
        m.commit(
            2,
            3,
            Some(&[EdgeDelta::Filed {
                kind: "k".into(),
                source: "A".into(),
                target: "B".into(),
                seq: 5,
                weight: Some(2.0),
            }]),
        );
        assert_eq!(m.lists.values().next().unwrap()[0].weight(), Some(2.0));
        m.commit(
            3,
            4,
            Some(&[EdgeDelta::Unfiled { kind: "k".into(), source: "A".into(), target: "B".into(), seq: 5 }]),
        );
        assert_eq!(m.held(), 0, "the list the entry's unfiling empties is dropped");
        // From another base, another process's commit between: emptied
        m.commit(9, 10, Some(&[]));
        assert_eq!((m.sequence(), m.held()), (None, 0));
        // A commit that can't say what it filed: emptied
        let mut m = mirror();
        m.commit(1, 2, None);
        assert_eq!((m.sequence(), m.held()), (None, 0));
    }

    /// A list of one entry for `entity`, out to `far`, as a drain reading it from the store fills one
    fn fill(m: &mut EdgeMirror, entity: &str, far: &str) {
        let key = (m.name(entity), m.name("k"), true);
        let far = m.name(far);
        m.lists.insert(key, vec![Entry::new(1, far, None)]);
        m.entries += 1;
    }

    #[test]
    fn names_no_list_mentions_are_let_go_once_more_entries_were_removed_than_are_held_and_the_lists_read_the_same() {
        let mut m = mirror();
        m.commit(1, 2, Some(&[filed(5)]));
        // Entities whose lists were read and then emptied, as deleted ones are
        for (i, seq) in (0..2 * REMOVED_KEPT).zip(2u64..) {
            let (entity, far) = (format!("X{i}"), format!("Y{i}"));
            fill(&mut m, &entity, &far);
            let unfiled = EdgeDelta::Unfiled { kind: "k".into(), source: entity, target: far, seq: 1 };
            m.commit(seq, seq + 1, Some(&[unfiled]));
        }
        // Two names an emptied list, and at most `REMOVED_KEPT` lists emptied since they were last counted
        assert!(m.ids.len() <= 2 * REMOVED_KEPT + 8, "{} names kept", m.ids.len());
        assert_eq!(m.held(), 1);
        let (a, k) = (m.known("A").unwrap(), m.known("k").unwrap());
        let list = &m.lists[&(a, k, true)];
        assert_eq!(list.iter().map(|e| &*m.ids[e.far as usize]).collect::<Vec<_>>(), ["B"]);
    }

    #[test]
    fn a_drain_filling_lists_leaves_the_names_as_they_are_however_many_it_adds() {
        let mut m = mirror();
        for i in 0..4 * REMOVED_KEPT {
            fill(&mut m, &format!("X{i}"), &format!("Y{i}"));
        }
        let names = m.ids.clone();
        // The drain's own commit, which files nothing, and one that removes one entry
        m.commit(1, 2, Some(&[]));
        let unfiled = EdgeDelta::Unfiled { kind: "k".into(), source: "X0".into(), target: "Y0".into(), seq: 1 };
        m.commit(2, 3, Some(&[unfiled]));
        assert!(m.ids == names, "the names were counted afresh with nothing to let go");
        assert_eq!((m.held(), m.entries), (4 * REMOVED_KEPT, 4 * REMOVED_KEPT - 1));
    }

    #[test]
    fn a_reader_at_another_sequence_empties_it_and_one_at_its_own_keeps_it() {
        let mut m = mirror();
        m.at(1);
        assert_eq!(m.held(), 1);
        m.at(7);
        assert_eq!((m.sequence(), m.held()), (Some(7), 0));
    }
}
