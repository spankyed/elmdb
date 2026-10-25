//! The engine: entities, attributes and relations held to a schema, written in batches inside units
//! (`docs/engine.md`).
//!
//! A batch written on its own is a unit of its own (`Database::write`). A unit can also stay open across calls
//! (`Database::begin`), as a caller's transaction that runs its own code between writes needs: its writes read their
//! own, a savepoint inside it rolls back alone, and its deferred checks run as it commits.

pub mod admin;
mod apply;
pub mod batch;
mod builder;
pub mod changes;
pub mod derive;
mod entities;
mod ids;
mod keys;
pub mod maintenance;
mod numbering;
pub mod plan;
mod reads;
mod relations;
mod text;
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
use unit::{Check, Read as _, Txn, UnitState};

/// A value's msgpack bytes
pub(crate) fn encode_value(value: &rmpv::Value) -> Vec<u8> {
    values::encode(value)
}

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

/// Reads from their msgpack bytes, the whole buffer checked against the value spec first: a value outside it is refused
/// as such (`VALUE_INVALID`), and a read that isn't one as `BATCH_INVALID`
pub fn decode_reads(bytes: &[u8]) -> Result<plan::Reads> {
    crate::format::value::validate(bytes)
        .map_err(|e| Error::new(e.code, format!("The reads aren't valid: {}", e.message)))?;
    rmp_serde::from_slice(bytes)
        .map_err(|e| Error::new(ErrorCode::BatchInvalid, format!("The reads don't decode: {e}")))
}

/// A batch from its msgpack bytes, the whole buffer checked against the value spec first: a value outside it is refused
/// as such (`VALUE_INVALID`), and an operation that isn't one as `BATCH_INVALID`
pub fn decode_batch(bytes: &[u8]) -> Result<Batch> {
    crate::format::value::validate(bytes)
        .map_err(|e| Error::new(e.code, format!("The batch isn't valid: {}", e.message)))?;
    rmp_serde::from_slice(bytes)
        .map_err(|e| Error::new(ErrorCode::BatchInvalid, format!("The batch doesn't decode: {e}")))
}

/// One value index's state
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    #[serde(rename = "type")]
    pub entity_type: String,
    pub index: String,
    pub built: bool,
    /// Why its build failed, such as a unique index over rows holding the same values
    pub failure: Option<String>,
}

/// Deletes the data of every index a schema change drops or rebuilds, with its marker, in the unit that stores the
/// schema
pub(crate) fn drop_index_data(
    db: crate::store::Db,
    txn: &mut RwTxn<'_>,
    changes: &[crate::schema::SchemaChange],
) -> Result<()> {
    use crate::schema::Effect;
    for change in changes.iter().filter(|c| matches!(c.effect, Effect::Drops | Effect::Rebuilds)) {
        let parts: Vec<&str> = change.path.split('.').collect();
        if let ["types", entity_type, "indexes", index] = parts.as_slice() {
            let (low, high) = crate::format::key::prefix_range(&keys::index_range(entity_type, index, &[]));
            let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
            let doomed: Vec<Vec<u8>> =
                db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
            for key in doomed {
                db.delete(txn, &key)?;
            }
            for range in keys::text_index_ranges(entity_type, index) {
                let (low, high) = crate::format::key::prefix_range(&range);
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                let doomed: Vec<Vec<u8>> =
                    db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
                for key in doomed {
                    db.delete(txn, &key)?;
                }
            }
            db.delete(txn, &keys::text_stats(entity_type, index))?;
            db.delete(txn, &keys::index_built(entity_type, index))?;
        }
    }
    Ok(())
}

/// Marks built every unbuilt index of a type that has no entities, and answers the rest, which need a build
pub(crate) fn settle_indexes(store: &Store, schema: &Schema) -> Result<Vec<(String, String)>> {
    let (empty, unbuilt) = store.read(|txn, db| {
        let s = unit::Snapshot { txn, db, schema };
        let mut empty = Vec::new();
        let mut unbuilt = Vec::new();
        for entity_type in schema.types.keys() {
            let mut has_entities = false;
            for trashed in [false, true] {
                s.visit(&keys::type_range(trashed, entity_type), |_, _| {
                    has_entities = true;
                    false
                })?;
            }
            for index in derive::index_names(schema, entity_type) {
                if !derive::is_built(&s, entity_type, &index)? {
                    if has_entities { &mut unbuilt } else { &mut empty }.push((entity_type.clone(), index));
                }
            }
        }
        Ok((empty, unbuilt))
    })?;
    if !empty.is_empty() && !store.is_read_only() {
        store.write(
            |txn, db| {
                for (entity_type, index) in &empty {
                    db.put(txn, &keys::index_built(entity_type, index), &values::encode(&true.into()))?;
                }
                Ok(())
            },
            // Opening leaves nothing owed, as the schema's write doesn't
            true,
        )?;
    }
    Ok(unbuilt)
}

pub(crate) use builder::Builder;

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

    /// Compares every derived key with what primary data derives, and every row with the schema's rules
    pub fn check(&self) -> Result<derive::CheckReport> {
        let store = self.store()?;
        let schema = self.engine_schema();
        store.read(|txn, db| derive::check(&unit::Snapshot { txn, db, schema: &schema }))
    }

    /// Deletes derived data and writes what primary data derives, for every index or, with one, that index, in one
    /// durable unit
    pub fn rebuild(&self, index: Option<(&str, &str)>) -> Result<()> {
        self.admin_unit(|t| derive::rebuild(t, index))
    }

    /// Deletes what a removed declaration left, in one durable unit; refuses a name the schema still declares
    pub fn purge(&self, what: &admin::Purge) -> Result<()> {
        self.admin_unit(|t| admin::purge(t, what))
    }

    /// Moves a field's values, on every entity of a type, to another name the type declares
    pub fn rename_field(&self, entity_type: &str, from: &str, to: &str) -> Result<()> {
        self.admin_unit(|t| admin::rename_field(t, entity_type, from, to))
    }

    /// Moves every relation of a kind to another kind the schema declares
    pub fn rename_relation(&self, from: &str, to: &str) -> Result<()> {
        self.admin_unit(|t| admin::rename_relation(t, from, to))
    }

    /// Renames a type, rewriting its entities' ids, the relations and declared `ref` fields naming them, and its
    /// sequences
    pub fn rename_type(&self, from: &str, to: &str) -> Result<()> {
        self.admin_unit(|t| admin::rename_type(t, from, to))
    }

    pub(crate) fn admin_load(&self, loads: &[maintenance::Load]) -> Result<()> {
        self.admin_unit(|t| {
            t.state.unchecked = true;
            maintenance::load(t, loads)
        })
    }

    fn admin_unit(&self, f: impl Fn(&mut Txn<'_, '_>) -> Result<()>) -> Result<()> {
        let schema = self.writing_schema();
        let store = self.store()?;
        let now = now_ms();
        store.write(
            |txn, db| {
                let mut state = UnitState::default();
                f(&mut Txn { txn, db, state: &mut state, schema: &schema, now })
            },
            true,
        )
    }

    /// Blocks until every index waiting to be built is built or has failed
    pub fn wait_for_index_builds(&self) {
        if let Some(builder) = self.builder().as_ref() {
            builder.wait();
        }
    }

    /// Every value index of every declared type: whether it is built, and why it couldn't be when a build failed
    pub fn index_status(&self) -> Result<Vec<IndexStatus>> {
        let store = self.store()?;
        let schema = self.engine_schema();
        let failures = self.builder().as_ref().map(|b| b.failures()).unwrap_or_default();
        store.read(|txn, db| {
            let s = unit::Snapshot { txn, db, schema: &schema };
            let mut out = Vec::new();
            for entity_type in schema.types.keys() {
                for index in entities::value_indexes(&schema, entity_type) {
                    let failure = failures.iter().find(|f| f.entity_type == *entity_type && f.index == index.name);
                    out.push(IndexStatus {
                        entity_type: entity_type.clone(),
                        index: index.name.clone(),
                        built: derive::is_built(&s, entity_type, &index.name)?,
                        failure: failure.map(|f| f.message.clone()),
                    });
                }
            }
            Ok(out)
        })
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
        let schema = self.writing_schema();
        let store = self.store()?;
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
        // Held until the unit is open, so a schema change either finishes first or is refused for the open unit
        let held = self.writing_schema();
        let store = self.store()?;
        let (txn, gate) = store.begin()?;
        let schema = Arc::clone(&held);
        drop(held);
        // SAFETY: the transaction borrows the store's environment. `Unit` holds the store, in a field declared after
        // the transaction's, so the transaction is dropped first and the environment outlives it
        let txn: RwTxn<'static> = unsafe { std::mem::transmute::<RwTxn<'_>, RwTxn<'static>>(txn) };
        Ok(Unit {
            txn: Some(txn),
            state: UnitState::default(),
            poisoned: None,
            _gate: gate,
            schema,
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
