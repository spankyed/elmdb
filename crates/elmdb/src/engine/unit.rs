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
    levels: Vec<Level>,
    pub(super) checks: Vec<Check>,
    pub(super) events: Vec<Event>,
}

impl UnitState {
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
    fn schema(&self) -> &Schema;

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

    fn schema(&self) -> &Schema {
        self.schema
    }
}

impl Read for Txn<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(self.txn, key)?.map(<[u8]>::to_vec))
    }

    fn visit(&self, prefix: &[u8], visit: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        visit_in(self.db, self.txn, prefix, visit)
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
    fn record(&mut self, key: &[u8]) -> Result<()> {
        let Some(level) = self.state.levels.last_mut() else { return Ok(()) };
        if level.recorded.contains(key) {
            return Ok(());
        }
        let before = self.db.get(self.txn, key)?.map(<[u8]>::to_vec);
        let level = self.state.levels.last_mut().expect("checked above");
        level.recorded.insert(key.to_vec());
        level.undo.push((key.to_vec(), before));
        Ok(())
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.record(key)?;
        self.db.put(self.txn, key, value)?;
        Ok(())
    }

    pub fn del(&mut self, key: &[u8]) -> Result<()> {
        self.record(key)?;
        self.db.delete(self.txn, key)?;
        Ok(())
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
