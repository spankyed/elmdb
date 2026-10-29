//! A unit's write transaction as the engine writes through it: reads that see the unit's own writes, an undo log per
//! savepoint, the checks a unit defers to its commit, and the log of what it changed.

use std::collections::HashSet;

use heed::RwTxn;

use crate::error::Result;
use crate::format::key::prefix_range;
use crate::schema::Schema;
use crate::store::Db;

use super::changes::Event;

/// A check made when the outermost unit commits: what a unit may leave inconsistent part way, it may not commit
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    /// The relation, if it still exists, has both its ends
    Ends { relation: String, via: &'static str },
}

/// What a savepoint undoes: each key first written since it, with what the key held before (`None`: nothing)
#[derive(Debug, Default)]
struct Level {
    undo: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    recorded: HashSet<Vec<u8>>,
    events: usize,
    checks: usize,
}

/// What a unit carries from one call to the next: its savepoints, deferred checks and events
#[derive(Debug, Default)]
pub struct UnitState {
    /// Whether the unit loads data unchecked (`bulkLoad`, a dump's load): a unique index then takes what it is given
    pub unchecked: bool,
    levels: Vec<Level>,
    pub(super) checks: Vec<Check>,
    pub(super) events: Vec<Event>,
    /// Every key the unit wrote, while tracked reads are filed for a commit to be matched against; `None` when none
    /// were as the unit began, and its commit is then heard as writing everything. A savepoint rolled back keeps its
    /// keys, which can only name a token that didn't need to be, never miss one
    pub(super) written: Option<HashSet<Vec<u8>>>,
    /// How many reads of the store the unit's writes have made: gets, visits and the undo log's reads of a value before
    /// it is replaced. What a spec counts a write by
    #[cfg(test)]
    pub(crate) reads: std::cell::Cell<u64>,
}

impl UnitState {
    /// A unit's state, logging the keys it writes when `logging`
    pub fn new(logging: bool) -> Self {
        UnitState { written: logging.then(Default::default), ..Default::default() }
    }

    /// Opens a savepoint, returning its depth
    pub fn savepoint(&mut self) -> usize {
        self.levels.push(Level { events: self.events.len(), checks: self.checks.len(), ..Default::default() });
        self.levels.len()
    }

    pub fn depth(&self) -> usize {
        self.levels.len()
    }
}

/// What reading the store takes: a unit's transaction, which sees its own writes, or a read snapshot
pub trait Read {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Visits the entries under `prefix` in key order until `visit` answers false
    fn visit(&self, prefix: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()>;
    /// Visits the entries whose key starts with `bytes`, which a key part left open ends (`KeyWriter::string_prefix`),
    /// in key order until `visit` answers false
    fn visit_starting(&self, bytes: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()>;
    /// Visits the entries under `prefix` whose key is `from` or after it, in key order until `visit` answers false: a
    /// walk that skips what it knows can't match seeks with it, rather than reading every entry
    fn visit_from(&self, prefix: &[u8], from: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()>;
    /// Visits the entries under `prefix` from the last, in reverse key order, until `visit` answers false: a walk that
    /// wants a range's last entries reads none before them
    fn visit_back(&self, prefix: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()>;
    /// Fallible streaming visit: stop at the first consumer error without collecting the range.
    fn try_visit(&self, prefix: &[u8], mut each: impl FnMut(&[u8], &[u8]) -> Result<()>) -> Result<()> {
        let mut result = Ok(());
        self.visit(prefix, |key, value| {
            result = each(key, value);
            result.is_ok()
        })?;
        result
    }

    fn schema(&self) -> &Schema;
    /// The transaction and the databases themselves, for what reads them through another library: hannoy's graphs
    fn raw(&self) -> (&heed::RoTxn<'_, heed::AnyTls>, Db);
    /// Says that what was read depends on every key under `prefix`, though nothing here read them: what a read through
    /// `raw` does, which a tracked read records (`tracking::Recording`)
    fn note(&self, _prefix: &[u8]) {}

    fn contains(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Every entry under `prefix`, in key order, collected so the caller may write while it goes through them
    fn scan(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.visit(prefix, |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            true
        })?;
        Ok(out)
    }
}

fn visit_in(
    db: Db,
    txn: &heed::RoTxn<'_, heed::WithoutTls>,
    prefix: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> bool,
) -> Result<()> {
    let (low, high) = prefix_range(prefix);
    let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
    for entry in db.range(txn, &range)? {
        let (k, v) = entry?;
        if !visit(k, v) {
            break;
        }
    }
    Ok(())
}

/// The entries under `prefix` from `from` on: the later of the two to the prefix's end
fn visit_from_in(
    db: Db,
    txn: &heed::RoTxn<'_, heed::WithoutTls>,
    prefix: &[u8],
    from: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> bool,
) -> Result<()> {
    let (low, high) = prefix_range(prefix);
    let start = if from > low.as_slice() { from } else { low.as_slice() };
    if start >= high.as_slice() {
        return Ok(());
    }
    let range = (std::ops::Bound::Included(start), std::ops::Bound::Excluded(high.as_slice()));
    for entry in db.range(txn, &range)? {
        let (k, v) = entry?;
        if !visit(k, v) {
            break;
        }
    }
    Ok(())
}

/// The entries under `prefix`, from the last
fn visit_back_in(
    db: Db,
    txn: &heed::RoTxn<'_, heed::WithoutTls>,
    prefix: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> bool,
) -> Result<()> {
    let (low, high) = prefix_range(prefix);
    let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
    for entry in db.rev_range(txn, &range)? {
        let (k, v) = entry?;
        if !visit(k, v) {
            break;
        }
    }
    Ok(())
}

/// The entries whose keys start with `bytes`: from `bytes` to its successor, its last byte below `0xFF` raised by one.
/// An escaped string's bytes end in no `0xFF`, which UTF-8 never holds
fn visit_starting_in(
    db: Db,
    txn: &heed::RoTxn<'_, heed::WithoutTls>,
    bytes: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> bool,
) -> Result<()> {
    let mut high = bytes.to_vec();
    while high.last() == Some(&0xFF) {
        high.pop();
    }
    let Some(last) = high.last_mut() else {
        return visit_in(db, txn, &[], visit);
    };
    *last += 1;
    let range = (std::ops::Bound::Included(bytes), std::ops::Bound::Excluded(high.as_slice()));
    for entry in db.range(txn, &range)? {
        let (k, v) = entry?;
        if !visit(k, v) {
            break;
        }
    }
    Ok(())
}

/// A read snapshot as the engine reads through it
pub struct Snapshot<'a, 'e> {
    pub txn: &'a heed::RoTxn<'e, heed::WithoutTls>,
    pub db: Db,
    pub schema: &'a Schema,
}

impl Read for Snapshot<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(self.txn, key)?.map(<[u8]>::to_vec))
    }

    fn visit(&self, prefix: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        visit_in(self.db, self.txn, prefix, visit)
    }

    fn visit_starting(&self, bytes: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        visit_starting_in(self.db, self.txn, bytes, visit)
    }

    fn visit_from(&self, prefix: &[u8], from: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        visit_from_in(self.db, self.txn, prefix, from, visit)
    }

    fn visit_back(&self, prefix: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        visit_back_in(self.db, self.txn, prefix, visit)
    }

    fn raw(&self) -> (&heed::RoTxn<'_, heed::AnyTls>, Db) {
        (self.txn, self.db)
    }

    fn schema(&self) -> &Schema {
        self.schema
    }
}

impl Read for Txn<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.counted();
        Ok(self.db.get(self.txn, key)?.map(<[u8]>::to_vec))
    }

    fn visit(&self, prefix: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        self.counted();
        visit_in(self.db, self.txn, prefix, visit)
    }

    fn visit_starting(&self, bytes: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        self.counted();
        visit_starting_in(self.db, self.txn, bytes, visit)
    }

    fn visit_from(&self, prefix: &[u8], from: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        self.counted();
        visit_from_in(self.db, self.txn, prefix, from, visit)
    }

    fn visit_back(&self, prefix: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        self.counted();
        visit_back_in(self.db, self.txn, prefix, visit)
    }

    fn raw(&self) -> (&heed::RoTxn<'_, heed::AnyTls>, Db) {
        (self.txn, self.db)
    }

    fn schema(&self) -> &Schema {
        self.schema
    }
}

/// The transaction and the unit's state for the length of one call
pub struct Txn<'a, 'e> {
    pub txn: &'a mut RwTxn<'e>,
    pub db: Db,
    pub state: &'a mut UnitState,
    pub schema: &'a Schema,
    /// The time this call's writes are stamped with
    pub now: f64,
}

impl Txn<'_, '_> {
    /// One read of the store, counted for a spec
    fn counted(&self) {
        #[cfg(test)]
        self.state.reads.set(self.state.reads.get() + 1);
    }

    fn record(&mut self, key: &[u8]) -> Result<()> {
        let Some(level) = self.state.levels.last_mut() else { return Ok(()) };
        if level.recorded.contains(key) {
            return Ok(());
        }
        self.counted();
        let before = self.db.get(self.txn, key)?.map(<[u8]>::to_vec);
        let level = self.state.levels.last_mut().expect("checked above");
        level.recorded.insert(key.to_vec());
        level.undo.push((key.to_vec(), before));
        Ok(())
    }

    fn log(&mut self, key: &[u8]) {
        if let Some(written) = &mut self.state.written {
            written.insert(key.to_vec());
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.record(key)?;
        self.log(key);
        self.db.put(self.txn, key, value)?;
        Ok(())
    }

    /// Records `before` as what `key` held, which the caller has just read, so the undo log needn't read it again
    fn record_known(&mut self, key: &[u8], before: Option<&[u8]>) {
        let Some(level) = self.state.levels.last_mut() else { return };
        if level.recorded.insert(key.to_vec()) {
            level.undo.push((key.to_vec(), before.map(<[u8]>::to_vec)));
        }
    }

    /// `put`, given what `key` holds now, as the caller read it in this call
    pub fn put_over(&mut self, key: &[u8], value: &[u8], before: Option<&[u8]>) -> Result<()> {
        self.record_known(key, before);
        self.log(key);
        self.db.put(self.txn, key, value)?;
        Ok(())
    }

    /// `del`, given what `key` holds now, as the caller read it in this call
    pub fn del_over(&mut self, key: &[u8], before: Option<&[u8]>) -> Result<()> {
        self.record_known(key, before);
        self.log(key);
        self.db.delete(self.txn, key)?;
        Ok(())
    }

    pub fn del(&mut self, key: &[u8]) -> Result<()> {
        self.record(key)?;
        self.log(key);
        self.db.delete(self.txn, key)?;
        Ok(())
    }

    /// Delete bounded batches of keys, seeking from the beginning after each batch has been removed. Values are
    /// never copied, and already removed rows are not revisited. Preserve normal undo/tracking through del.
    pub(crate) fn clear_prefix(&mut self, prefix: &[u8]) -> Result<()> {
        loop {
            let mut keys = Vec::new();
            self.visit(prefix, |key, _| {
                keys.push(key.to_vec());
                keys.len() < 1000
            })?;
            if keys.is_empty() {
                return Ok(());
            }
            for key in keys {
                self.del(&key)?;
            }
        }
    }

    pub fn event(&mut self, event: Event) {
        self.state.events.push(event);
    }

    pub fn defer(&mut self, check: Check) {
        self.state.checks.push(check);
    }

    /// Undoes everything since the savepoint at `depth` and closes it and every one inside it
    pub fn rollback_to(&mut self, depth: usize) -> Result<()> {
        while self.state.levels.len() >= depth && depth > 0 {
            let level = self.state.levels.pop().expect("a level at or past depth");
            for (key, before) in level.undo.into_iter().rev() {
                match before {
                    Some(value) => self.db.put(self.txn, &key, &value)?,
                    None => {
                        self.db.delete(self.txn, &key)?;
                    }
                }
            }
            self.state.events.truncate(level.events);
            self.state.checks.truncate(level.checks);
        }
        Ok(())
    }

    /// Closes the savepoint at `depth` and every one inside it, keeping their writes in the unit around them
    pub fn release(&mut self, depth: usize) {
        while self.state.levels.len() >= depth && depth > 0 {
            let level = self.state.levels.pop().expect("a level at or past depth");
            if let Some(parent) = self.state.levels.last_mut() {
                for (key, before) in level.undo {
                    if parent.recorded.insert(key.clone()) {
                        parent.undo.push((key, before));
                    }
                }
            }
        }
    }
}
