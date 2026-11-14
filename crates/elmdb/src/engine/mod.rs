//! The engine: entities, attributes and relations held to a schema, written in batches inside units
//! (`docs/architecture/engine.md`).
//!
//! A batch written on its own is a unit of its own (`Database::write`). A unit can also stay open across calls
//! (`Database::begin`), as a caller's transaction that runs its own code between writes needs: its writes read their
//! own, a savepoint inside it rolls back alone, and its deferred checks run as it commits.

pub mod admin;
mod algorithms;
mod apply;
pub mod batch;
mod builder;
pub mod changed;
pub mod changes;
pub mod derive;
mod entities;
mod ids;
mod keys;
pub mod maintenance;
pub(crate) mod mirror;
mod numbering;
mod order;
mod places;
pub mod plan;
mod ranks;
mod reads;
mod relations;
mod sum;
mod text;
pub mod tracking;
mod trigram;
pub(crate) mod unit;
mod values;
mod vector;
pub mod watch;

use std::marker::PhantomData;
use std::sync::Arc;

use heed::RwTxn;
use parking_lot::lock_api::ArcRwLockReadGuard;
use parking_lot::{RawRwLockRecursive, RwLock};

use crate::database::{Database, EngineSchema};
use crate::diagnostics::Op;
use crate::error::{Error, ErrorCode, Result};
use crate::schema::Schema;
use crate::store::{Commit, Store};
use batch::{Batch, BatchResult};
use changes::ChangeSet;
pub(crate) use reads::collator;
use unit::{Check, Read as _, Txn, UnitState};

/// Finalizes every user-visible commit before reporting its sync outcome. `None` invalidates every owner for
/// operations whose writes were not logged key by key.
fn finish_commit(
    tracks: &watch::SharedTracks,
    store: &Store,
    schema: &RwLock<Arc<EngineSchema>>,
    mut changes: ChangeSet,
    written: Option<&std::collections::HashSet<Vec<u8>>>,
    commit: Commit,
) -> Result<ChangeSet> {
    let touched = tracks.lock().touched(written);
    changes.sequence = commit.sequence;
    changes.touched = touched.tokens;
    watch::deliver(tracks, store, schema, &touched.watches);
    commit.finish()?;
    Ok(changes)
}

/// How the builder's chunks commit, an index's and a rank index's: as a write does, each telling the watches and the
/// tracked reads whose reads it wrote into
pub(crate) fn builder_commits(
    tracks: watch::SharedTracks,
    store: Arc<Store>,
    observing: Arc<RwLock<Arc<EngineSchema>>>,
    schema: Arc<EngineSchema>,
) -> builder::Commits {
    Arc::new(move |chunk| {
        // mutant-skip: delete ! in builder_commits
        // The rank index (`docs/archive/roadmap/rank-index.md`). How the builder's chunks commit: logging narrows which watches
        // run again, and a watch delivers only an answer that changed, so a commit that logs nothing tells every watch
        // and each hears the same
        let logging = !tracks.lock().is_empty();
        let ((done, written), commit) = store.write_committed(
            |txn, db| {
                schema.validate(txn, db)?;
                let mut state = UnitState::new(logging);
                state.mirror = Some(store.mirror());
                let mut t = Txn { txn, db, state: &mut state, schema: &schema, now: now_ms() };
                let done = chunk(&mut t)?;
                store.deposit(std::mem::take(&mut state.edges));
                Ok((done, state.written.take()))
            },
            false,
        )?;
        let changes = finish_commit(&tracks, &store, &observing, ChangeSet::default(), written.as_ref(), commit)?;
        if !changes.touched.is_empty() {
            let listener = tracks.lock().builder_commit_listener();
            if let Some(listener) = listener {
                // A callback of the database, on the builder's own thread: a close it asks for is refused as one
                // asked for by a watch's listener is, whichever builder is the database's by then. It is let go of
                // inside the callback too, as a watch's listener is: what it holds may be the last of the database,
                // whose close would wait for this thread
                let (sequence, touched) = (changes.sequence, changes.touched);
                let heard = watch::as_callback(&tracks, move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(sequence, touched)))
                });
                // One that panics is heard no more, as a watch whose listener panics is ended: the builder goes on,
                // where the panic would have ended its thread with builds left that nothing waits out
                if heard.is_err() {
                    tracks.lock().hear_builder_commits(None).expect("dropping the listener refuses nothing");
                }
            }
        }
        Ok(done)
    })
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

/// One index's state: a type's, or a rank index
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    /// The type the index is on; none for a rank index, which spans types
    #[serde(rename = "type")]
    pub entity_type: Option<String>,
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
        if let ["ranks", name] = parts.as_slice() {
            for prefix in [keys::rank_nodes(name), keys::rank_orders(name), keys::rank_queue(name)] {
                let (low, high) = crate::format::key::prefix_range(&prefix);
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                let doomed: Vec<Vec<u8>> =
                    db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
                for key in doomed {
                    db.delete(txn, &key)?;
                }
            }
            db.delete(txn, &keys::rank_totals(name))?;
            continue;
        }
        if let ["types", entity_type, "indexes", index] = parts.as_slice() {
            // Each range of a value index's keys, the blocks of their places, and the key that is a range's own first:
            // an index on one field keeps the entities without a value under it
            for own in keys::value_index_ranges(entity_type, index) {
                for under in [own.clone(), keys::index_blocks(&own)] {
                    let (low, high) = crate::format::key::prefix_range(&under);
                    let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                    let doomed: Vec<Vec<u8>> =
                        db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
                    for key in doomed {
                        db.delete(txn, &key)?;
                    }
                }
                db.delete(txn, &own)?;
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
            let (low, high) = crate::format::key::prefix_range(&keys::trigram_index(entity_type, index));
            let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
            let doomed: Vec<Vec<u8>> =
                db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
            for key in doomed {
                db.delete(txn, &key)?;
            }
            // A vector index's derived keys, the embeddings stored for it, and its graph
            for prefix in [
                crate::format::Prefix::Vector,
                crate::format::Prefix::VectorChunk,
                crate::format::Prefix::VectorPending,
                crate::format::Prefix::VectorItem,
                crate::format::Prefix::VectorItemOf,
                crate::format::Prefix::VectorJournal,
            ] {
                let (low, high) =
                    crate::format::key::prefix_range(&keys::vector_index_range(prefix, entity_type, index));
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                let doomed: Vec<Vec<u8>> =
                    db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
                for key in doomed {
                    db.delete(txn, &key)?;
                }
            }
            if let Some(graph) = db.get(txn, &keys::vector_graph(entity_type, index))? {
                let number = values::decode(graph)?.as_array().and_then(|a| a.first()).and_then(rmpv::Value::as_u64);
                if let Some(number) = number {
                    vector::clear_graph(txn, db, number as u16)?;
                }
                db.delete(txn, &keys::vector_graph(entity_type, index))?;
            }
            // A value index's order goes with it: its keys under every build, and its marker
            let (low, high) = crate::format::key::prefix_range(&keys::order_of(entity_type, index));
            let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
            let doomed: Vec<Vec<u8>> =
                db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
            for key in doomed {
                db.delete(txn, &key)?;
            }
            db.delete(txn, &keys::order_marker(entity_type, index))?;
            db.delete(txn, &keys::index_built(entity_type, index))?;
        }
    }
    Ok(())
}

/// What the stored data holds that changing the schema from `stored` to `given` answers to (`schema::Held`): one seek
/// per type with entities, one per type for its codes' prefix, and a walk of the relations only when `given` adds a
/// relation kind, stopping once each added kind is found
pub(crate) fn held_in(
    txn: &heed::RoTxn<'_, heed::WithoutTls>,
    db: crate::store::Db,
    stored: &EngineSchema,
    given: &Schema,
) -> Result<crate::schema::Held> {
    use crate::format::key::{Part, decode, prefix_range};
    use std::ops::Bound;
    stored.validate(txn, db)?;
    let mut held = crate::schema::Held::default();
    for trashed in [false, true] {
        let (mut low, high) = prefix_range(&keys::all_of_types(trashed));
        loop {
            let range = (Bound::Included(low.as_slice()), Bound::Excluded(high.as_slice()));
            let Some(entry) = db.range(txn, &range)?.next() else { break };
            let (key, _) = entry?;
            let Some(Part::String(entity_type)) = decode(key)?.into_iter().nth(1) else { break };
            low = prefix_range(&keys::type_range(trashed, &entity_type)).1;
            held.types.insert(entity_type);
        }
    }
    for entity_type in &held.types {
        let range = prefix_range(&keys::index_range(entity_type, keys::SHORT_CODE_INDEX, &[]));
        let range = (Bound::Included(range.0.as_slice()), Bound::Excluded(range.1.as_slice()));
        if let Some(entry) = db.range(txn, &range)?.next()
            && let Some(Part::String(code)) = decode(entry?.0)?.into_iter().nth(3)
            && let Some((prefix, _)) = code.rsplit_once('-')
        {
            held.short_codes.insert(entity_type.clone(), prefix.to_owned());
        }
    }
    let mut sought: std::collections::BTreeSet<&String> =
        given.relations.keys().filter(|kind| !stored.relations.contains_key(*kind)).collect();
    if !sought.is_empty() {
        let s = unit::Snapshot { txn, db, schema: stored };
        let mut failed = None;
        s.visit(&keys::all_relations(), |_, bytes| {
            match rmp_serde::from_slice::<relations::Stored>(bytes) {
                Ok(relation) => {
                    if let Some(kind) = sought.take(&relation.kind) {
                        held.kinds.insert(kind.clone());
                    }
                }
                Err(e) => failed = Some(e),
            }
            failed.is_none() && !sought.is_empty()
        })?;
        if let Some(e) = failed {
            return Err(Error::new(ErrorCode::Corrupted, format!("A relation's row doesn't read: {e}")));
        }
    }
    Ok(held)
}

type IndexList = Vec<Work>;

/// What a unit leaves the builder as it commits: the rank indexes it left pushes for, and the orders of value
/// indexes it found without a marker of this elmdb's as it wrote a string to them
fn work_at_commit(t: &mut Txn<'_, '_>) -> Result<IndexList> {
    let mut work: IndexList = ranks::push_at_commit(t)?.into_iter().map(|name| Work::Rank { name }).collect();
    let owed = std::mem::take(&mut t.state.orders_owed);
    work.extend(owed.into_iter().map(|(entity_type, index)| Work::Order { entity_type, index }));
    Ok(work)
}

fn unbuilt_indexes(r: &impl unit::Read) -> Result<(IndexList, IndexList)> {
    let mut empty = Vec::new();
    let mut unbuilt = Vec::new();
    let has_entities = |entity_type: &str, trashed_too: bool| -> Result<bool> {
        let mut found = false;
        for trashed in if trashed_too { &[false, true][..] } else { &[false][..] } {
            r.visit(&keys::type_range(*trashed, entity_type), |_, _| {
                found = true;
                false
            })?;
        }
        Ok(found)
    };
    for entity_type in r.schema().types.keys() {
        let held = has_entities(entity_type, true)?;
        for index in derive::index_names(r.schema(), entity_type) {
            if !derive::is_built(r, entity_type, &index)? {
                let work = Work::Index { entity_type: entity_type.clone(), index };
                if held { &mut unbuilt } else { &mut empty }.push(work);
            }
        }
        // A built value index's order that no read of this elmdb's trusts is the builder's to build
        for index in entities::value_indexes(r.schema(), entity_type).into_iter().filter(entities::ValueIndex::accounts)
        {
            if derive::is_built(r, entity_type, &index.name)? && order::readable(r, entity_type, &index.name)?.is_none()
            {
                unbuilt.push(Work::Order { entity_type: entity_type.clone(), index: index.name });
            }
        }
    }
    // A rank index's nodes are live entities: one over types with none is built empty
    for name in ranks::unfinished(r)? {
        let def = &r.schema().ranks[&name];
        let mut held = false;
        for entity_type in &def.types {
            held |= has_entities(entity_type, false)?;
        }
        let built = ranks::totals(r, &name)?.stage != ranks::Stage::Joining;
        if held || built { &mut unbuilt } else { &mut empty }.push(Work::Rank { name });
    }
    Ok((empty, unbuilt))
}

/// Marks empty types' indexes built inside the mutation that changes their contents or declaration.
pub(crate) fn settle_indexes_in(t: &mut Txn<'_, '_>) -> Result<IndexList> {
    changed::settle(t.txn, t.db, t.schema)?;
    let (empty, unbuilt) = unbuilt_indexes(t)?;
    for work in empty {
        match work {
            Work::Index { entity_type, index } => {
                t.put(&keys::index_built(&entity_type, &index), &values::encode(&true.into()))?;
                // Built with nothing in it, a value index's order is whole with it, of no key: the writes that
                // fill the one keep the other from here
                let declared = entities::value_indexes(t.schema, &entity_type).into_iter().find(|i| i.name == index);
                if declared.is_some_and(|found| found.accounts()) {
                    order::begin_whole(t, &entity_type, &index)?;
                }
            }
            Work::Order { .. } => {}
            Work::Rank { name } => ranks::rebuild(t, &name)?,
        }
    }
    Ok(unbuilt)
}

/// Read pending index work without committing marker repairs while a caller is reporting a refused mutation.
pub(crate) fn remaining_indexes(store: &Store, schema: &EngineSchema) -> Result<IndexList> {
    failure_point!(INDEX_REFRESH, Error::new(ErrorCode::Io, "injected index refresh failure"));
    store.read(|txn, db| {
        schema.validate(txn, db)?;
        let (empty, unbuilt) = unbuilt_indexes(&unit::Snapshot { txn, db, schema })?;
        Ok(empty.into_iter().chain(unbuilt).collect())
    })
}

/// Settle empty-type markers with their committed sync outcome kept separate from the pending work.
pub(crate) fn settle_indexes_committed(store: &Store, schema: &EngineSchema) -> Result<(IndexList, Option<Commit>)> {
    let (empty, unbuilt) = store.read(|txn, db| {
        schema.validate(txn, db)?;
        unbuilt_indexes(&unit::Snapshot { txn, db, schema })
    })?;
    let order_settled = !store.read(|txn, db| changed::needs_settling(txn, db, schema))?;
    if (!empty.is_empty() || !order_settled) && !store.is_read_only() {
        return store
            .write_committed(
                |txn, db| {
                    schema.validate(txn, db)?;
                    let mut state = UnitState::default();
                    let unbuilt = settle_indexes_in(&mut Txn { txn, db, state: &mut state, schema, now: 0.0 })?;
                    store.deposit(std::mem::take(&mut state.edges));
                    Ok(unbuilt)
                },
                true,
            )
            .map(|(unbuilt, commit)| (unbuilt, Some(commit)));
    }
    Ok((unbuilt, None))
}

pub(crate) use builder::{Builder, Work};

/// A position as the binding hands it, msgpack of `changed::Position`
fn position(bytes: &[u8]) -> Result<changed::Position> {
    rmp_serde::from_slice(bytes)
        .map_err(|error| Error::new(ErrorCode::BatchInvalid, format!("A position to read what changed from: {error}")))
}

/// What a write's caller gets back across the binding: each operation's answer, and the commit's change set
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Written<'a> {
    pub results: &'a [batch::OpResult],
    pub changes: &'a ChangeSet,
    /// The commit's sequence, as its change set says
    pub sequence: u64,
}

/// A write that has committed and is owed a sync. Dropped without a wait, nothing says whether it became durable
pub struct Unsynced {
    sequence: u64,
    store: Arc<Store>,
}

impl Unsynced {
    /// Asks for a sync and waits for it, answering as a durable write does: `NOT_DURABLE` when it failed. The sync is
    /// asked for here and not as the write commits, and covers every commit made before this call: of several writes
    /// waited for after the last of them, the first wait's sync is the others' too
    pub fn wait(self) -> Result<()> {
        let synced = self.store.durable().map_or(Ok(()), crate::store::sync::Durable::wait);
        Commit { sequence: self.sequence, synced }.finish()
    }
}

impl Database {
    /// `write` with the batch as msgpack bytes and the answer as msgpack bytes (`Written`), as the binding takes them
    pub fn write_bytes(&self, batch: &[u8], durable: bool) -> Result<Vec<u8>> {
        let batch = decode_batch(batch)?;
        let (results, changes) = self.write(&batch, durable)?;
        Ok(rmp_serde::to_vec_named(&Written {
            results: &results.results,
            changes: &changes,
            sequence: changes.sequence,
        })
        .expect("an answer serializes"))
    }

    /// `write_bytes`, answered once the commit has landed and before its sync, with the sync to wait for
    /// (`Unsynced::wait`): a caller with several writes in hand makes them all and waits once, where each made
    /// durable would wait for a sync of its own. Its watches and tracked reads are told as the commit lands, before
    /// the sync, where a durable write tells them after it
    pub fn write_bytes_unsynced(&self, batch: &[u8]) -> Result<(Vec<u8>, Unsynced)> {
        let batch = decode_batch(batch)?;
        // The store the write goes to, held until its sync is waited for: a close waits for it, as for a durable write
        let store = self.store()?;
        let (results, changes) = self.write(&batch, false)?;
        let answer = Written { results: &results.results, changes: &changes, sequence: changes.sequence };
        let answer = rmp_serde::to_vec_named(&answer).expect("an answer serializes");
        Ok((answer, Unsynced { sequence: changes.sequence, store }))
    }

    /// Compares every derived key with what primary data derives, and every row with the schema's rules
    pub fn check(&self) -> Result<derive::CheckReport> {
        let store = self.store()?;
        let schema = self.schema_guard();
        let mirror = store.mirror();
        store.read(|txn, db| {
            schema.validate(txn, db)?;
            let snapshot = unit::Snapshot { txn, db, schema: &schema };
            let mut report = derive::check(&snapshot)?;
            // The edge mirror, compared with the store only where both are at one commit's state
            let graph = mirror.lock();
            if graph.sequence() == Some(mirror::stored_sequence(&snapshot)?) {
                report.violations.extend(
                    graph
                        .differences(&snapshot)?
                        .into_iter()
                        .map(|message| derive::Violation { subject: "edge mirror".to_owned(), message }),
                );
            }
            Ok(report)
        })
    }

    /// Deletes derived data and writes what primary data derives, for every index or, with one, that index, in one
    /// durable unit
    pub fn rebuild(&self, index: Option<(&str, &str)>) -> Result<()> {
        let op = Op::rebuild(index);
        let result = op.in_scope(|| {
            if index.is_none() {
                self.store()?.make_room(REBUILD_ROOM)?;
            }
            self.admin_unit(|t| derive::rebuild(t, index))
        });
        op.ended(result, "durable")
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
        let store = self.store()?;
        self.admin_unit(|t| admin::rename_type(t, from, to, store.next_sequence(t.txn)?))
    }

    pub(crate) fn admin_load(&self, loads: &[maintenance::Load]) -> Result<()> {
        self.admin_unit(|t| {
            t.state.unchecked = true;
            maintenance::load(t, loads)
        })
    }

    pub(crate) fn admin_unit(&self, f: impl Fn(&mut Txn<'_, '_>) -> Result<()>) -> Result<()> {
        self.admin_unit_with_checkpoint(f, false)
    }

    pub(crate) fn admin_unit_with_checkpoint(
        &self,
        f: impl Fn(&mut Txn<'_, '_>) -> Result<()>,
        recycle: bool,
    ) -> Result<()> {
        let schema = self.schema_guard();
        let store = self.store()?;
        let builders = self.builders_of(Arc::clone(&store))?;
        let now = now_ms();
        let (waiting, mut commit) = store.write_committed(
            |txn, db| {
                schema.validate(txn, db)?;
                let mut state = UnitState::default();
                let mut t = Txn { txn, db, state: &mut state, schema: &schema, now };
                f(&mut t)?;
                vector::flush(&mut t)?;
                let waiting = work_at_commit(&mut t)?;
                store.deposit_touched(changed::Touched::of(&state.events, &schema));
                Ok(waiting)
            },
            !recycle,
        )?;
        if recycle {
            // Recycling is only a space/performance optimization. Even if its transaction aborts, sync the atomic
            // reset before reporting success; only that required sync determines its durability outcome.
            let _ = store.recycle_reset_pages();
            commit.synced = store.committed(true);
        }
        drop(schema);
        // As in `write`: queued before a failed sync is reported, the commit having landed
        let landed = self.finish_admin_commit(&store, commit);
        if let Some(builders) = builders {
            builders.queue(waiting);
        }
        landed
    }

    pub(crate) fn finish_admin_commit(&self, store: &Store, commit: Commit) -> Result<()> {
        finish_commit(&self.tracks, store, &self.observing_schema(), ChangeSet::default(), None, commit).map(|_| ())
    }

    /// The checkpoint named `name`, as the latest commit left it (`Unit::set_checkpoint`)
    pub fn checkpoint(&self, name: &str) -> Result<Option<Vec<u8>>> {
        checkpoint_name(name)?;
        self.store()?.read(|txn, db| Ok(db.get(txn, &keys::checkpoint(name))?.map(<[u8]>::to_vec)))
    }

    /// Whether another handle has committed since this one last asked, or since it opened: like SQLite's `data_version`,
    /// it moves for another's commits and never for this handle's own, its index builder's included
    pub fn committed_elsewhere(&self) -> Result<bool> {
        self.store()?.committed_elsewhere()
    }

    /// The entities of `types` that changed after `from`, in the order of the commits that changed them, at most
    /// `limit`, with the ones destroyed since, from one snapshot (`docs/architecture/engine.md`, Changed since)
    pub fn changed_since(&self, types: &[String], from: &changed::Position, limit: usize) -> Result<changed::Changed> {
        let schema = self.schema_guard();
        self.store()?.read(|txn, db| changed::since(txn, db, &schema, types, from, limit))
    }

    /// `changed_since` with its position and answer as msgpack bytes, as the binding takes them
    pub fn changed_since_bytes(&self, types: &[String], from: &[u8], limit: usize) -> Result<Vec<u8>> {
        let from = position(from)?;
        Ok(rmp_serde::to_vec_named(&self.changed_since(types, &from, limit)?).expect("an answer serializes"))
    }

    /// Blocks until every index waiting to be built is built or has failed. Refused while a unit is open
    /// (`UNIT_OPEN`): a build waits for the unit to end, and a unit open on the calling thread never would. Refused
    /// on the thread of the builder it would wait for (`CALLBACK_ACTIVE`), by a listener one of its commits
    /// delivered to: those builds go on once the listener returns. A listener that changed the schema waits for
    /// the builder started for it, which is another thread's
    pub fn wait_for_index_builds(&self) -> Result<()> {
        // Taken out of the slot, which stays free while this waits: a schema change or a close stops the builder
        let Some((waiter, here)) = self.builder().as_ref().map(|builder| (builder.waiter(), builder.on_its_thread()))
        else {
            return Ok(());
        };
        // On the builder's own thread, in a listener one of its commits delivered to, the builds wait for the
        // listener to return: the wait would be for itself
        if here {
            return Err(Error::new(
                ErrorCode::CallbackActive,
                "A listener is running on the builder's thread, and the builds wait for it to return: wait for them \
                 after it has",
            ));
        }
        if self.store()?.unit_is_open() {
            return Err(Error::new(
                ErrorCode::UnitOpen,
                "A unit is open, and an index build waits for it to end: commit or abort it, then wait for the builds",
            ));
        }
        waiter.wait();
        let failures = self.builder().as_ref().map(Builder::failures).unwrap_or_default();
        if failures.is_empty() {
            return Ok(());
        }
        let named: Vec<String> = failures
            .iter()
            .map(|f| match &f.work {
                Work::Index { entity_type, index } => format!("{entity_type}'s index {index}: {}", f.message),
                Work::Order { entity_type, index } => {
                    format!("the order of {entity_type}'s index {index}: {}", f.message)
                }
                Work::Rank { name } => format!("the rank index {name}: {}", f.message),
            })
            .collect();
        Err(Error::new(ErrorCode::IndexBuildFailed, format!("An index build failed: {}", named.join("; "))))
    }

    /// Every value index of every declared type, and every rank index: whether it is built, and why it couldn't be when
    /// a build failed
    pub fn index_status(&self) -> Result<Vec<IndexStatus>> {
        let store = self.store()?;
        let schema = self.schema_guard();
        let failures = self.builder().as_ref().map(|b| b.failures()).unwrap_or_default();
        store.read(|txn, db| {
            schema.validate(txn, db)?;
            let s = unit::Snapshot { txn, db, schema: &schema };
            let mut out = Vec::new();
            for entity_type in schema.types.keys() {
                for index in entities::value_indexes(&schema, entity_type) {
                    let failure = failures.iter().find(|f| {
                        matches!(&f.work, Work::Index { entity_type: t, index: i } if t == entity_type && *i == index.name)
                    });
                    out.push(IndexStatus {
                        entity_type: Some(entity_type.clone()),
                        index: index.name.clone(),
                        built: derive::is_built(&s, entity_type, &index.name)?,
                        failure: failure.map(|f| f.message.clone()),
                    });
                }
            }
            for name in schema.ranks.keys() {
                let failure = failures.iter().find(|f| matches!(&f.work, Work::Rank { name: n } if n == name));
                out.push(IndexStatus {
                    entity_type: None,
                    index: name.clone(),
                    built: ranks::is_built(&s, name)?,
                    failure: failure.map(|f| f.message.clone()),
                });
            }
            Ok(out)
        })
    }

    /// Answers reads from one snapshot of the last commit, as one msgpack envelope (`docs/architecture/engine.md`, Reads)
    pub fn read(&self, reads: &plan::Reads) -> Result<Vec<u8>> {
        let store = self.store()?;
        let schema = self.schema_guard();
        run_point!(READ_SNAPSHOT);
        let Some(token) = reads.track else {
            return store.read(|txn, db| {
                schema.validate(txn, db)?;
                reads::run(&unit::Snapshot { txn, db, schema: &schema }, &reads.reads)
            });
        };
        // Filed whether or not the reads failed: what they read before failing decides when they'd answer otherwise
        watch::read_tracked(&self.tracks, &store, &schema, reads, token)
    }

    /// Drops what the reads tracked under `token` touched: no commit names it after
    pub fn untrack(&self, token: u32) {
        self.tracks.lock().untrack(token);
    }

    /// How many owners have tracked reads filed, and how many keys and ranges they hold: `(owners, keys, ranges)`
    pub fn track_stats(&self) -> (usize, usize, usize) {
        self.tracks.lock().stats()
    }

    /// The last commit's sequence, or 0 before the first
    pub fn sequence(&self) -> Result<u64> {
        Ok(self.store()?.sequence())
    }

    /// Runs `reads` and hands `listener` the answer, then runs them again after each commit that writes into what they
    /// read, handing over each answer that differs from the one before. A listener is called after the commit, outside
    /// its transaction, on the thread that committed. A panicking listener is removed; an initial panic returns
    /// `CALLBACK_PANICKED`, while later panics do not change the committed operation's outcome
    pub fn watch(&self, reads: plan::Reads, listener: watch::Listener) -> Result<u64> {
        let store = self.store()?;
        watch::watch(&self.tracks, &store, &self.observing_schema(), reads, listener)
    }

    /// Ends a watch: once this returns, its listener isn't called again, and with `Ended` it has returned and been
    /// dropped; `Ending`, from inside a listener, says a delivery still holds it (`watch::unwatch`)
    pub fn unwatch(&self, id: u64) -> watch::Unwatched {
        watch::unwatch(&self.tracks, id)
    }

    /// `read` with the reads as msgpack bytes, as the binding takes them
    pub fn read_bytes(&self, reads: &[u8]) -> Result<Vec<u8>> {
        self.read(&decode_reads(reads)?)
    }

    /// Applies a batch as one unit, refusing all of it when any operation is refused. `durable` commits it with a sync
    pub fn write(&self, batch: &Batch, durable: bool) -> Result<(BatchResult, ChangeSet)> {
        let schema = self.schema_guard();
        let store = self.store()?;
        let builders = self.builders_of(Arc::clone(&store))?;
        let now = batch.now.unwrap_or_else(now_ms);
        let logging = !self.tracks.lock().is_empty();
        let ((results, changes, written, waiting), commit) = store.write_committed(
            |txn, db| {
                schema.validate(txn, db)?;
                let mut state = UnitState::new(logging);
                let mut t = Txn { txn, db, state: &mut state, schema: &schema, now };
                let results = apply_all(&mut t, batch)?;
                run_checks(&t)?;
                vector::flush(&mut t)?;
                let waiting = work_at_commit(&mut t)?;
                store.deposit(std::mem::take(&mut state.edges));
                let changes = ChangeSet::of(&state.events);
                store.deposit_touched(changed::Touched::of(&state.events, &schema));
                Ok((results, changes, state.written.take(), waiting))
            },
            durable,
        )?;
        drop(schema);
        // The commit has landed whatever its sync answers, so its rank work is queued before a failed sync is reported
        let landed = finish_commit(&self.tracks, &store, &self.observing_schema(), changes, written.as_ref(), commit);
        if let Some(builders) = builders {
            builders.queue(waiting);
        }
        Ok((results, landed?))
    }

    /// Runs `f` on a read snapshot of the last commit
    #[cfg(test)]
    pub(crate) fn snapshot<T>(&self, f: impl FnOnce(&unit::Snapshot<'_, '_>) -> Result<T>) -> Result<T> {
        let store = self.store()?;
        let schema = self.schema_guard();
        store.read(|txn, db| {
            schema.validate(txn, db)?;
            f(&unit::Snapshot { txn, db, schema: &schema })
        })
    }

    /// Opens a unit that stays open across calls until it is committed or aborted. One unit is open at a time, and
    /// while it is, `write` refuses (`UNIT_OPEN`): its writes go through the unit
    pub fn begin(&self) -> Result<Unit> {
        // Held until the unit is open, so a schema change either finishes first or is refused for the open unit
        let held = self.schema_guard();
        let store = self.store()?;
        // The unit's place is let go if no unit comes of it, refused or panicking, once the transaction and its
        // holds, declared after it, have gone
        let mut place = Place { store: &store, taken: false };
        let (txn, gate, writer) = store.begin()?;
        // Declared so that an unwind drops the transaction first, then the gate, as the unit's own end does
        let (writer, gate, txn) = (writer, gate, txn);
        place.taken = true;
        run_point!(UNIT_HELD);
        if let Err(error) = held.validate(&txn, store.db()) {
            drop(txn);
            drop(gate);
            writer.release();
            return Err(error);
        }
        let schema = Arc::clone(&held);
        drop(held);
        let builders = match self.builders() {
            Ok(builders) => builders,
            Err(error) => {
                drop(txn);
                drop(gate);
                writer.release();
                return Err(error);
            }
        };
        // SAFETY: the transaction borrows the store's environment. `Unit` holds the store, in a field declared after
        // the transaction's, so the transaction is dropped first and the environment outlives it
        let txn: RwTxn<'static> = unsafe { std::mem::transmute::<RwTxn<'_>, RwTxn<'static>>(txn) };
        let logging = !self.tracks.lock().is_empty();
        let (tracks, schemas) = (Arc::clone(&self.tracks), self.observing_schema());
        // From here the unit holds the place, and ends it
        place.taken = false;
        drop(place);
        Ok(Unit {
            txn: Some(txn),
            state: UnitState::new(logging),
            tracks,
            schemas,
            poisoned: None,
            gate: Some(gate),
            writer: Some(writer),
            schema,
            store,
            builders,
            ended: false,
            _thread: PhantomData,
        })
    }
}

/// A unit's place in the store while the unit is being made: ended when dropped while taken
struct Place<'a> {
    store: &'a Store,
    taken: bool,
}

impl Drop for Place<'_> {
    fn drop(&mut self) {
        if self.taken {
            self.store.end_unit();
        }
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
    /// The store's gate, held while the transaction is: a map growth waits for the unit to end
    gate: Option<ArcRwLockReadGuard<RawRwLockRecursive, ()>>,
    /// The writer's hold on the write transaction, let go once the transaction has ended and the store has accounted
    /// for its commit (`store/writers.rs`)
    writer: Option<crate::store::WriterLock>,
    schema: Arc<EngineSchema>,
    schemas: Arc<RwLock<Arc<EngineSchema>>>,
    store: Arc<Store>,
    tracks: watch::SharedTracks,
    /// The builder the rank indexes' pushes its commit leaves go to
    builders: Option<crate::database::Builders>,
    /// Whether the store has been told the unit ended: once, however it ends
    ended: bool,
    _thread: PhantomData<*const ()>,
}

/// A checkpoint is named by its reader, and every reader needs a name
fn checkpoint_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::new(ErrorCode::BatchInvalid, "A checkpoint has a name"));
    }
    Ok(())
}

impl Unit {
    /// Records `value` as the checkpoint `name`, in this unit, so it commits with the work it records or not at all: a
    /// reader's place in what it has read (CouchDB's local documents, Kafka's committed offsets). A checkpoint is no
    /// entity and changes no change order. A backup keeps it with the rows it describes; a dump leaves it out, and
    /// `reset` and `replace_with` drop it with them, so a reader starts again from nothing
    pub fn set_checkpoint(&mut self, name: &str, value: &[u8]) -> Result<()> {
        checkpoint_name(name)?;
        self.view(0.0)?.put(&keys::checkpoint(name), value)
    }

    /// The checkpoint named `name`, as this unit sees it
    pub fn checkpoint(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        checkpoint_name(name)?;
        let view = self.view(0.0)?;
        unit::Read::get(&view, &keys::checkpoint(name))
    }

    /// What this unit has changed so far, which something running inside it may need to act on before it commits
    pub fn pending_changes(&mut self) -> Result<ChangeSet> {
        let view = self.view(0.0)?;
        Ok(ChangeSet::of(&view.state.events))
    }

    /// `pending_changes` as msgpack bytes, as the binding takes it
    pub fn pending_changes_bytes(&mut self) -> Result<Vec<u8>> {
        Ok(rmp_serde::to_vec_named(&self.pending_changes()?).expect("a change set serializes"))
    }

    /// `Database::changed_since` read in this unit, before its own writes are stamped: what it has written so far is in
    /// `pending_changes`, and takes `next_sequence` as it commits
    pub fn changed_since(
        &mut self,
        types: &[String],
        from: &changed::Position,
        limit: usize,
    ) -> Result<changed::Changed> {
        let view = self.view(0.0)?;
        changed::since(view.txn, view.db, view.schema, types, from, limit)
    }

    /// `changed_since` with its position and answer as msgpack bytes, as the binding takes them
    pub fn changed_since_bytes(&mut self, types: &[String], from: &[u8], limit: usize) -> Result<Vec<u8>> {
        let from = position(from)?;
        Ok(rmp_serde::to_vec_named(&self.changed_since(types, &from, limit)?).expect("an answer serializes"))
    }

    /// The sequence this unit commits as: what it writes is stamped with it. The unit holds the write transaction, so
    /// no other commit can take it first
    pub fn next_sequence(&mut self) -> Result<u64> {
        // What a poisoned or ended unit refuses, as its other calls do
        self.view(0.0)?;
        let txn = self.txn.as_ref().ok_or_else(|| Error::new(ErrorCode::Closed, "The unit has ended"))?;
        self.store.next_sequence(txn)
    }

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
                // LMDB can't go on with a transaction a write filled the map in, and the rollback itself fails then
                let rolled_back = t.rollback_to(depth);
                if error.code == ErrorCode::MapFull || rolled_back.is_err() {
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
        let tracks = Arc::clone(&self.tracks);
        let t = self.view(0.0)?;
        let Some(token) = reads.track else { return reads::run(&t, &reads.reads) };
        let recording = tracking::Recording::new(&t);
        let answer = reads::run(&recording, &reads.reads);
        tracks.lock().file(token, recording.into_reads());
        answer
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
        let checked = self.view(0.0).and_then(|mut t| {
            run_checks(&t)?;
            vector::flush(&mut t)?;
            work_at_commit(&mut t)
        });
        let waiting = match checked {
            Ok(waiting) => waiting,
            Err(error) => {
                self.abort();
                return Err(error);
            }
        };
        let changes = ChangeSet::of(&self.state.events);
        let txn = self.txn.take().expect("an open unit has its transaction");
        run_point!(UNIT_COMMITTING);
        self.store.deposit(std::mem::take(&mut self.state.edges));
        self.store.deposit_touched(changed::Touched::of(&self.state.events, &self.schema));
        let committed = self.store.commit_txn(txn);
        self.gate = None;
        self.writer = None;
        self.ended = true;
        self.store.end_unit();
        let sequence = committed?;
        let synced = self.store.committed(durable);
        // As in `write`: the commit has landed whatever its sync answers, so its rank work is queued before a failed sync
        let landed = finish_commit(
            &self.tracks,
            &self.store,
            &self.schemas,
            changes,
            self.state.written.as_ref(),
            Commit { sequence, synced },
        );
        if let Some(builders) = &self.builders {
            builders.queue(waiting);
        }
        landed
    }

    /// Ends the unit, keeping nothing of it
    pub fn abort(&mut self) {
        // The transaction first, and then its holds. A commit that panicked once it had taken the transaction has
        // not told the store, which is told here as the unit is dropped
        self.txn = None;
        if !self.ended {
            self.gate = None;
            self.writer = None;
            self.ended = true;
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

/// How many times what the database holds a full rebuild asks the map to be before it begins: it deletes every
/// derived key and writes each again in one unit, where a page freed is no room until the unit commits
const REBUILD_ROOM: usize = 3;

#[cfg(test)]
mod tests;
