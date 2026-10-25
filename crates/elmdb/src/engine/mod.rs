//! The engine: entities, attributes and relations held to a schema, written in batches inside units
//! (`docs/engine.md`).
//!
//! A batch written on its own is a unit of its own (`Database::write`). A unit can also stay open across calls
//! (`Database::begin`), as a caller's transaction that runs its own code between writes needs: its writes read their
//! own, a savepoint inside it rolls back alone, and its deferred checks run as it commits.

mod apply;
pub mod batch;
pub mod changes;
mod entities;
mod ids;
mod keys;
mod numbering;
pub mod plan;
mod reads;
mod relations;
mod unit;
mod values;

use std::marker::PhantomData;
use std::sync::Arc;

use heed::RwTxn;
use parking_lot::RawRwLock;
use parking_lot::lock_api::ArcRwLockReadGuard;

use crate::database::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::schema::Schema;
use crate::store::Store;
use batch::{Batch, BatchResult};
use changes::ChangeSet;
use unit::{Check, Txn, UnitState};

/// Milliseconds since the epoch, the engine's clock when a batch brings no time
fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

fn apply_all(t: &mut Txn<'_, '_>, batch: &Batch) -> Result<BatchResult> {
    let mut results = Vec::with_capacity(batch.ops.len());
    for op in &batch.ops {
        results.push(apply::apply(t, op)?);
    }
    Ok(BatchResult { results })
}

/// The checks a unit deferred to its commit
fn run_checks(t: &Txn<'_, '_>) -> Result<()> {
    for check in &t.state.checks {
        match check {
            Check::Ends { relation, via } => relations::check_ends(t, relation, via)?,
        }
    }
    Ok(())
}

/// Reads from their msgpack bytes, the whole buffer checked against the value spec first
pub fn decode_reads(bytes: &[u8]) -> Result<plan::Reads> {
    crate::format::value::decode(bytes)
        .map_err(|e| Error::new(ErrorCode::BatchInvalid, format!("The reads aren't valid: {}", e.message)))?;
    rmp_serde::from_slice(bytes)
        .map_err(|e| Error::new(ErrorCode::BatchInvalid, format!("The reads don't decode: {e}")))
}

/// A batch from its msgpack bytes, the whole buffer checked against the value spec first
pub fn decode_batch(bytes: &[u8]) -> Result<Batch> {
    crate::format::value::decode(bytes)
        .map_err(|e| Error::new(ErrorCode::BatchInvalid, format!("The batch isn't valid: {}", e.message)))?;
    rmp_serde::from_slice(bytes)
        .map_err(|e| Error::new(ErrorCode::BatchInvalid, format!("The batch doesn't decode: {e}")))
}

/// What a write's caller gets back across the binding: each operation's answer, and the commit's change set
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Written<'a> {
    pub results: &'a [batch::OpResult],
    pub changes: &'a ChangeSet,
}

impl Database {
    /// `write` with the batch as msgpack bytes and the answer as msgpack bytes (`Written`), as the binding takes them
    pub fn write_bytes(&self, batch: &[u8], durable: bool) -> Result<Vec<u8>> {
        let batch = decode_batch(batch)?;
        let (results, changes) = self.write(&batch, durable)?;
        Ok(rmp_serde::to_vec_named(&Written { results: &results.results, changes: &changes })
            .expect("an answer serializes"))
    }

    /// Answers reads from one snapshot of the last commit, as one msgpack envelope (`docs/engine.md`, Reads)
    pub fn read(&self, reads: &plan::Reads) -> Result<Vec<u8>> {
        let store = self.store()?;
        let schema = self.engine_schema();
        store.read(|txn, db| reads::run(&unit::Snapshot { txn, db, schema: &schema }, &reads.reads))
    }

    /// `read` with the reads as msgpack bytes, as the binding takes them
    pub fn read_bytes(&self, reads: &[u8]) -> Result<Vec<u8>> {
        self.read(&decode_reads(reads)?)
    }

    /// Applies a batch as one unit, refusing all of it when any operation is refused. `durable` commits it with a sync
    pub fn write(&self, batch: &Batch, durable: bool) -> Result<(BatchResult, ChangeSet)> {
        let store = self.store()?;
        let schema = self.engine_schema();
        let now = batch.now.unwrap_or_else(now_ms);
        store.write(
            |txn, db| {
                let mut state = UnitState::default();
                let mut t = Txn { txn, db, state: &mut state, schema: &schema, now };
                let results = apply_all(&mut t, batch)?;
                run_checks(&t)?;
                Ok((results, ChangeSet::of(&state.events)))
            },
            durable,
        )
    }

    /// Runs `f` on a read snapshot of the last commit
    #[cfg(test)]
    pub(crate) fn snapshot<T>(&self, f: impl FnOnce(&unit::Snapshot<'_, '_>) -> Result<T>) -> Result<T> {
        let store = self.store()?;
        let schema = self.engine_schema();
        store.read(|txn, db| f(&unit::Snapshot { txn, db, schema: &schema }))
    }

    /// Opens a unit that stays open across calls until it is committed or aborted. One unit is open at a time, and
    /// while it is, `write` refuses (`UNIT_OPEN`): its writes go through the unit
    pub fn begin(&self) -> Result<Unit> {
        let store = self.store()?;
        let (txn, gate) = store.begin()?;
        // SAFETY: the transaction borrows the store's environment. `Unit` holds the store, in a field declared after
        // the transaction's, so the transaction is dropped first and the environment outlives it
        let txn: RwTxn<'static> = unsafe { std::mem::transmute::<RwTxn<'_>, RwTxn<'static>>(txn) };
        Ok(Unit {
            txn: Some(txn),
            state: UnitState::default(),
            poisoned: None,
            _gate: gate,
            schema: self.engine_schema(),
            store,
            _thread: PhantomData,
        })
    }
}

/// A unit open across calls. Its write transaction belongs to the thread that began it, so it can't be sent to
/// another. Dropped without a commit, it is aborted
pub struct Unit {
    // Field order is drop order: the transaction before the gate and the store it borrows
    txn: Option<RwTxn<'static>>,
    state: UnitState,
    /// Why the unit can't commit: LMDB can't go on with a transaction a write filled the map in
    poisoned: Option<Error>,
    _gate: ArcRwLockReadGuard<RawRwLock, ()>,
    schema: Arc<Schema>,
    store: Arc<Store>,
    _thread: PhantomData<*const ()>,
}

impl Unit {
    fn view(&mut self, now: f64) -> Result<Txn<'_, 'static>> {
        if let Some(error) = &self.poisoned {
            return Err(Error::new(
                error.code,
                format!("The unit can't go on, and must be aborted: {}", error.message),
            ));
        }
        let txn = self.txn.as_mut().ok_or_else(|| Error::new(ErrorCode::Closed, "The unit has ended"))?;
        Ok(Txn { txn, db: self.store.db(), state: &mut self.state, schema: &self.schema, now })
    }

    /// Applies a batch in the unit, keeping none of it when any operation is refused
    pub fn write(&mut self, batch: &Batch) -> Result<BatchResult> {
        let now = batch.now.unwrap_or_else(now_ms);
        let mut t = self.view(now)?;
        let depth = t.state.savepoint();
        match apply_all(&mut t, batch) {
            Ok(results) => {
                t.release(depth);
                Ok(results)
            }
            Err(error) => {
                t.rollback_to(depth)?;
                if error.code == ErrorCode::MapFull {
                    self.poisoned = Some(error.clone());
                }
                Err(error)
            }
        }
    }

    /// Runs `f` on the unit's transaction, which sees the unit's own writes
    #[cfg(test)]
    pub(crate) fn read_with<T>(&mut self, f: impl FnOnce(&Txn<'_, 'static>) -> Result<T>) -> Result<T> {
        let t = self.view(0.0)?;
        f(&t)
    }

    /// `write` with the batch and the answer as msgpack bytes, the answer each operation's (`BatchResult`)
    pub fn write_bytes(&mut self, batch: &[u8]) -> Result<Vec<u8>> {
        let batch = decode_batch(batch)?;
        let results = self.write(&batch)?;
        Ok(rmp_serde::to_vec_named(&results).expect("an answer serializes"))
    }

    /// Answers reads from the unit's transaction, which sees the unit's own writes
    pub fn read(&mut self, reads: &plan::Reads) -> Result<Vec<u8>> {
        let t = self.view(0.0)?;
        reads::run(&t, &reads.reads)
    }

    /// `read` with the reads as msgpack bytes
    pub fn read_bytes(&mut self, reads: &[u8]) -> Result<Vec<u8>> {
        let reads = decode_reads(reads)?;
        self.read(&reads)
    }

    /// Opens a savepoint, returning its depth for `rollback_to` and `release`
    pub fn savepoint(&mut self) -> Result<usize> {
        Ok(self.view(0.0)?.state.savepoint())
    }

    /// Undoes everything since the savepoint at `depth`, closing it
    pub fn rollback_to(&mut self, depth: usize) -> Result<()> {
        self.view(0.0)?.rollback_to(depth)
    }

    /// Closes the savepoint at `depth`, keeping its writes in the unit around it
    pub fn release(&mut self, depth: usize) -> Result<()> {
        self.view(0.0)?.release(depth);
        Ok(())
    }

    /// Commits the unit, after its deferred checks; a check that fails aborts the whole unit. `durable` waits for the
    /// commit's sync
    pub fn commit(mut self, durable: bool) -> Result<ChangeSet> {
        let checked = self.view(0.0).and_then(|t| run_checks(&t));
        if let Err(error) = checked {
            self.abort();
            return Err(error);
        }
        let changes = ChangeSet::of(&self.state.events);
        let txn = self.txn.take().expect("an open unit has its transaction");
        let committed = self.store.commit_txn(txn);
        self.store.end_unit();
        committed?;
        self.store.committed(durable)?;
        Ok(changes)
    }

    /// Ends the unit, keeping nothing of it
    pub fn abort(&mut self) {
        if self.txn.take().is_some() {
            self.store.end_unit();
        }
    }

    pub fn is_open(&self) -> bool {
        self.txn.is_some()
    }

    /// The savepoints open in the unit
    pub fn depth(&self) -> usize {
        self.state.depth()
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
mod tests;
