//! The engine: entities, attributes and relations held to a schema, written in batches inside units
//! (`docs/architecture/engine.md`).
//!
//! A batch written on its own is a unit of its own (`Database::write`). A unit can also stay open across calls
//! (`Database::begin`), as a caller's transaction that runs its own code between writes needs: its writes read their
//! own, and a savepoint inside it rolls back alone.

pub mod admin;
mod algorithms;
pub mod answer;
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
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use heed::{RoTxn, RwTxn, WithoutTls};
use parking_lot::RwLock;

use crate::database::{Database, EngineSchema};
use crate::diagnostics::Op;
use crate::error::{Error, ErrorCode, Result};
use crate::schema::Schema;
use crate::store::{Commit, Db, Store};
use crate::sync::PlainMutex;
use batch::{Batch, BatchResult};
use changes::ChangeSet;
pub(crate) use reads::collator;
use unit::{Read as _, Txn, UnitState};

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
    let touched = {
        let mut held = tracks.lock();
        held.note_commit(commit.sequence, written);
        held.touched(written)
    };
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

/// A read asked with `Database::read_at_call`, which `Database::answer_call_read` answers on any thread
pub struct CallRead {
    /// The read as asked: waiting, or run at the call (`None`), or refused there
    asked: Result<Option<crate::store::CallRead>>,
    /// What the read answered, once it ran: here, at the call, or in a growth of the map that ran it
    answered: Arc<PlainMutex<Option<std::thread::Result<Ran>>>>,
    /// A tracked read's token, noted for until it is filed
    tracked: Option<watch::Waiting>,
}

/// A read's answer, and, when it was tracked, what it read
type Ran = (Result<Vec<u8>>, Option<tracking::ReadSet>);

/// What `Database::answer_call_read` answers
pub struct CallAnswer {
    /// The reads' answer, as `read` gives it
    pub answer: Result<Vec<u8>>,
    /// For a tracked read, the last commit since its call that wrote into what it read, by its sequence, and the
    /// read's token: no commit's result named the token, since the read wasn't filed when it committed
    /// (`Database::tell_touched`)
    pub touched: Option<(u64, u32)>,
}

/// Runs a read asked at a call on its snapshot, recording what it read when it is tracked
fn run_at_call(txn: &RoTxn<'_, WithoutTls>, db: Db, schema: &EngineSchema, reads: &plan::Reads, tracking: bool) -> Ran {
    if let Err(error) = schema.validate(txn, db) {
        return (Err(error), tracking.then(tracking::ReadSet::everything));
    }
    let snapshot = unit::Snapshot { txn, db, schema };
    if tracking {
        let (answer, read) = watch::run_recorded(&snapshot, reads);
        return (answer, Some(read));
    }
    (reads::run(&snapshot, &reads.reads), None)
}

/// Milliseconds since the epoch, the engine's clock when a batch brings no time
fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

fn apply_all(t: &mut Txn<'_, '_>, batch: &Batch) -> Result<BatchResult> {
    apply::finite_time("The batch's `now`", batch.now)?;
    let mut results = Vec::with_capacity(batch.ops.len());
    for op in &batch.ops {
        results.push(apply::apply(t, op)?);
    }
    Ok(BatchResult { results })
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
    /// Whether the order a value index keeps of its strings is one a read of this database takes an `orderBy` or a
    /// string `range` from: filed by this elmdb and whole. False while it is built, and for one another elmdb
    /// filed, whose orders are sorted as they are read until this one's builder has built them. None for an index
    /// that keeps no order: a rank index, and a type's own indexes of short codes and roles
    pub ordered: Option<bool>,
    /// The bytes of a string's sort key the order's keys hold under the type's and the index's names, before the
    /// values of a compound index's leading fields take their share: a string whose sort key is longer is ordered
    /// by the bytes it keeps, then by the collator among those keeping the same. 0 where the names leave none, and
    /// the index's strings are then sorted when they are read. None for an index that keeps no order
    pub room: Option<u32>,
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
            db.delete(txn, &keys::index_reached(entity_type, index))?;
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
            // And so is one it does trust that holds keys under another build, which a build stopped before its
            // last unit left
            if !derive::is_built(r, entity_type, &index.name)? {
                continue;
            }
            let owed = match order::readable(r, entity_type, &index.name)? {
                Some(build) => order::stale(r, entity_type, &index.name, build)?,
                None => true,
            };
            if owed {
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
                derive::mark(t, &entity_type, &index, true)?;
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

/// What a wait for the builds queued again, each time the building went idle owing it: how many times in a row it was
/// the same work with no commit since, which is a builder that ran it and left it unbuilt. Work owed anew, by commits
/// landing meanwhile, is progress and not counted
#[derive(Default)]
struct QueuedAgain {
    times: u32,
    before: Option<(Vec<Work>, u64)>,
}

impl QueuedAgain {
    /// Notes `owed`, as of the store's commit `sequence`, about to be queued again: whether it is the third time in a
    /// row with nothing committed between, the builder having run it twice over and left it unbuilt
    fn stuck(&mut self, owed: &[Work], sequence: u64) -> bool {
        let unchanged = self.before.as_ref().is_some_and(|(before, at)| before == owed && *at == sequence);
        self.times = if unchanged { self.times + 1 } else { 0 };
        self.before = Some((owed.to_vec(), sequence));
        self.times >= 2
    }
}

/// The work's indexes, named for a message
fn named(work: &[Work]) -> String {
    let names: Vec<String> = work
        .iter()
        .map(|work| match work {
            Work::Index { entity_type, index } => format!("{entity_type}'s index {index}"),
            Work::Order { entity_type, index } => format!("the order of {entity_type}'s index {index}"),
            Work::Rank { name } => format!("the rank index {name}"),
        })
        .collect();
    names.join(", ")
}

/// Refuses a wait for the index builds while another process has replaced every row and this handle hasn't adopted
/// that (`DATA_REPLACED`): the builder lets go of work it can't commit over such rows, so nothing would be built
fn builds_can_commit(store: &Store) -> Result<()> {
    match store.replaced_elsewhere()? {
        None => Ok(()),
        Some(_) => Err(Error::new(
            ErrorCode::DataReplaced,
            "Another process replaced every row in this database, and no index build commits until this handle has \
             taken that: adopt the replacement, or open the database again, then wait for the builds",
        )),
    }
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

pub(crate) use builder::{Building, Commits as BuilderCommits, Target, Waited, Work};

/// A position as the binding hands it, msgpack of `changed::Position`
fn position(bytes: &[u8]) -> Result<changed::Position> {
    rmp_serde::from_slice(bytes)
        .map_err(|error| Error::new(ErrorCode::BatchInvalid, format!("A position to read what changed from: {error}")))
}

/// What a write's caller gets back across the binding: each operation's answer, the commit's change set and its
/// sequence
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Written<'a> {
    pub results: &'a [batch::OpResult],
    pub changes: &'a ChangeSet,
    /// The commit's sequence, as its change set says
    pub sequence: u64,
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
            // One rebuild at a time: two that each found the map short would each grow it
            let _one_at_a_time = self.rebuilding.lock();
            if index.is_none() {
                self.store()?.make_room(REBUILD_ROOM)?;
            }
            run_point!(REBUILD_ROOM_MADE);
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

    /// Blocks until every index waiting to be built is built or has failed, and answers `Ok` only where the database
    /// says so: once the builder is idle, what is unbuilt is read, and what of it is owed, but for builds that failed
    /// and an order's build that is put off, is queued and waited for again. A builder that runs such work twice, with
    /// nothing committed between, and leaves it unbuilt disagrees with the database: the work is listed as failed,
    /// which this answers (`INDEX_BUILD_FAILED`). A build that failed is tried again where no thread was building as this was asked.
    ///
    /// Refused while a unit is open and there is something to build (`UNIT_OPEN`): a build waits for the unit to end,
    /// and a unit open on the calling thread never would. Refused on the builder's thread (`CALLBACK_ACTIVE`), by a
    /// listener one of its commits delivered to: those builds go on once the listener returns. Refused the same in
    /// any listener while a close has the builder stopped and hasn't returned: it may be waiting for the listener.
    /// Refused while another process's replacement of every row is not adopted (`DATA_REPLACED`): no build commits
    /// over rows that are gone, and adopting the replacement queues what it left unbuilt. Refused where a close at
    /// exit gave up and let the builder go with something left unbuilt (`BUILDS_STOPPED`). The read of what is
    /// unbuilt is refused as any read is (`SCHEMA_CHANGED`, `READERS_FULL`). A read-only database builds nothing and
    /// answers at once.
    ///
    /// The order a value index keeps of its strings is waited for as an index is, but for a build of it that was
    /// put off: one another elmdb's writes ended, which this builder begins again after a wait that doubles
    /// (`docs/architecture/engine.md`, Reads). The index is built and a read of it answers meanwhile, sorting what
    /// it would have read in order, and `index_status` says the order isn't one to read (`ordered`).
    ///
    /// A schema change tells the builder that what it builds to is changing, and then what the change leaves to
    /// build: a wait under way waits through both (`Core::waited`), and answers for the schema the database holds
    /// when it returns. A close stops the builder's thread, and a wait under way waits for it to say what became of
    /// the database: closed, the wait is told so (`CLOSED`), as it is of a database closed before it; left open by a
    /// close that gave up, it goes on to what that close queues again
    pub fn wait_for_index_builds(&self) -> Result<()> {
        let mut queued_again = QueuedAgain::default();
        // A build that failed is tried again where no thread was building as this was asked: the data may have
        // changed since, as the next open would try it. With a thread at work, its failures stand until it has more
        // to build, and are what this answers
        let mut try_failed = !self.building.running();
        loop {
            let store = self.store()?;
            // On the builder's own thread, in a listener one of its commits delivered to, the builds wait for the
            // listener to return: the wait would be for itself
            if self.building.on_its_thread() {
                return Err(Error::new(
                    ErrorCode::CallbackActive,
                    "A listener is running on the builder's thread, and the builds wait for it to return: wait for them \
                     after it has",
                ));
            }
            // A read-only database builds nothing
            if self.is_read_only() {
                return Ok(());
            }
            builds_can_commit(&store)?;
            // A build waits for a unit to end, and one open on this thread never would
            if store.unit_is_open() && (self.building.running() || self.building.has_work()) {
                return Err(Error::new(
                    ErrorCode::UnitOpen,
                    "A unit is open, and an index build waits for it to end: commit or abort it, then wait for the builds",
                ));
            }
            // A thread for what is queued, where none runs: work a stopped thread put back waits for one
            if let Some(builders) = self.builders_of(Arc::clone(&store))? {
                builders.start(Vec::new())?;
            }
            // Let go of before the wait: a close waits for every hold on the store but its own
            drop(store);
            // A close under way is waited through, to its closing the database or giving up, but by a listener: the
            // close may be waiting for this thread, for the builder it stopped, whose commit delivered to the listener
            // running here, or for what the operation the listener is called from holds
            match self.building.wait(!watch::in_callback(&self.tracks)) {
                Waited::Idle => {}
                // Work queued and no thread to take it: one is started for it, at the top
                Waited::Unrun => continue,
                // Held up by what refused its work with nothing made, the builder waits it out: told so, the caller
                // tries again
                Waited::Held(held) => {
                    return Err(Error::new(
                        held.code,
                        format!(
                            "The index builds wait for what refused their last write, and go on once it passes: call \
                             this again after a pause to wait on. What refused it: {}",
                            held.message
                        ),
                    ));
                }
                Waited::Closing => {
                    return Err(Error::new(
                        ErrorCode::CallbackActive,
                        "A close is under way, which a listener running on this thread may keep from ending, and the \
                         index builds wait for it: wait for them after the listener returns",
                    ));
                }
            }
            // The building is idle. Whether all is built is the database's to say, not the building's: what it says
            // is unbuilt, but for what failed and an order's build that is put off (`Core::waited`), is owed. A
            // close that closed the database meanwhile is said as it is of one closed before
            let store = self.store()?;
            let unbuilt: Vec<Work> = remaining_indexes(&store, &self.schema_guard())?
                .into_iter()
                .filter(|work| !self.building.put_off(work))
                .collect();
            // Nothing more is built here, a close at exit having let the builder go: what is unbuilt, failed or not,
            // stays so in this process
            if self.building.closed() && !unbuilt.is_empty() {
                return Err(Error::new(
                    ErrorCode::BuildsStopped,
                    format!(
                        "A close at exit gave up on this database and let its builder go, and {} left unbuilt: reads \
                         of it walk the rows, and the next open tries to build it",
                        named(&unbuilt)
                    ),
                ));
            }
            let failures = self.building.failures();
            let owed: Vec<Work> = unbuilt
                .into_iter()
                .filter(|work| try_failed || !failures.iter().any(|failure| failure.work == *work))
                .collect();
            try_failed = false;
            if owed.is_empty() {
                if !failures.is_empty() {
                    break;
                }
                // The builder lets go of work it can't commit over rows another process replaced, and fails nothing
                return builds_can_commit(&store);
            }
            // Owed and idle: no thread ran it, a close or a stop having let it go, or commits landed and owed it anew,
            // or a thread ran it and left it unbuilt. The last, twice over with nothing committed between, is a builder
            // that disagrees with the database: it is listed as failed, which this then answers
            if queued_again.stuck(&owed, store.sequence()) {
                for work in owed {
                    self.building.fail(work, "the builder ran it and left it unbuilt, saying nothing".to_owned());
                }
                break;
            }
            if let Some(builders) = self.builders_of(store)? {
                builders.start(owed)?;
            }
        }
        let failures = self.building.failures();
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
        let failures = self.building.failures();
        store.read(|txn, db| {
            schema.validate(txn, db)?;
            let s = unit::Snapshot { txn, db, schema: &schema };
            let mut out = Vec::new();
            for entity_type in schema.types.keys() {
                for index in entities::value_indexes(&schema, entity_type) {
                    let failure = failures.iter().find(|f| {
                        matches!(&f.work, Work::Index { entity_type: t, index: i } if t == entity_type && *i == index.name)
                    });
                    let (ordered, room) = match index.accounts() {
                        true => {
                            let room = keys::order_room(entity_type, &index.name, &[]).unwrap_or(0);
                            (Some(order::readable(&s, entity_type, &index.name)?.is_some()), Some(room as u32))
                        }
                        false => (None, None),
                    };
                    out.push(IndexStatus {
                        entity_type: Some(entity_type.clone()),
                        index: index.name.clone(),
                        built: derive::is_built(&s, entity_type, &index.name)?,
                        failure: failure.map(|f| f.message.clone()),
                        ordered,
                        room,
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
                    ordered: None,
                    room: None,
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

    /// Says the notice of a restore an open reported (`OpenReport::restored`) was read, so that no open reports it
    /// after; refused on a read-only handle
    pub fn acknowledge_restore(&self) -> Result<()> {
        self.store()?.acknowledge_restore()
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

    /// `read`, answered by `answer_call_read` on whichever thread calls it, from the snapshot of the last commit as it
    /// is at this call: a commit made before the answer, here or in another process, changes nothing it reads
    /// (`docs/architecture/engine.md`, Reads). The snapshot is held until the read runs, unless as many are held for
    /// such reads as may be, when the read runs here. A tracked one is refused (`NO_LISTENER`) while nothing listens for
    /// the builder's commits (`hear_builder_commits`), which is where it is told of a commit that wrote into it before it
    /// was filed
    pub fn read_at_call(&self, reads: plan::Reads) -> CallRead {
        let answered = Arc::default();
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return CallRead { asked: Err(error), answered, tracked: None },
        };
        // A commit that writes into a tracked read before it is filed is told to the listener of the builder's commits
        // alone, which lets it go only at a close: without one, its caller would keep an answer it took to be tracked
        if reads.track.is_some() && self.tracks.lock().builder_commit_listener().is_none() {
            let refused = Error::new(
                ErrorCode::NoListener,
                "A tracked read answered later is told of a commit that writes into it before it is filed only through \
                 the listener of the builder's commits: register one (`hearBuilderCommits`), or track the read with \
                 `read`",
            );
            return CallRead { asked: Err(refused), answered, tracked: None };
        }
        // Held until the snapshot is taken, so the schema read with it is the one it was written under
        let schema = self.schema_guard();
        // Noted for before the snapshot is taken, so that every commit it doesn't hold is noted
        let tracked = reads.track.map(|token| watch::Waiting::new(&self.tracks, token));
        let read = {
            let answered = Arc::clone(&answered);
            let schema = Arc::clone(&*schema);
            let tracking = tracked.is_some();
            move |txn: &RoTxn<'static, WithoutTls>, db: Db| {
                let ran = catch_unwind(AssertUnwindSafe(|| run_at_call(txn, db, &schema, &reads, tracking)));
                *answered.lock() = Some(ran);
            }
        };
        let asked = store.ask(read);
        drop(schema);
        CallRead { asked, answered, tracked }
    }

    /// `read_at_call` with the reads as msgpack bytes, as the binding takes them
    pub fn read_at_call_bytes(&self, reads: &[u8]) -> CallRead {
        match decode_reads(reads) {
            Ok(reads) => self.read_at_call(reads),
            Err(error) => CallRead { asked: Err(error), answered: Arc::default(), tracked: None },
        }
    }

    /// Answers a read asked with `read_at_call`, running it here unless it ran at the call or a growth of the map ran
    /// it. Refused `CLOSED` when the database closed before it ran, and `DATABASE_MISMATCH` when another database asked
    /// it. A tracked read is filed here, as `read` files one,
    /// and answered with the last commit since its call that wrote into what it read, for the caller to tell once it
    /// has handed the answer over (`tell_touched`)
    pub fn answer_call_read(&self, read: CallRead) -> CallAnswer {
        let CallRead { asked, answered, tracked } = read;
        let ran = asked.and_then(|asked| match asked {
            Some(waiting) => self.store()?.run_asked(waiting),
            None => Ok(()),
        });
        let taken = answered.lock().take();
        let (answer, reads) = match (taken, ran) {
            (Some(Ok(ran)), _) => ran,
            (Some(Err(panic)), _) => std::panic::resume_unwind(panic),
            // The database closed first, or the read was answered through another: nothing was read, and nothing is filed
            (None, Err(error)) if matches!(error.code, ErrorCode::Closed | ErrorCode::DatabaseMismatch) => {
                return CallAnswer { answer: Err(error), touched: None };
            }
            // No snapshot could be taken: filed as `read` files a read that failed so, for everything
            (None, Err(error)) => (Err(error), tracked.is_some().then(tracking::ReadSet::everything)),
            (None, Ok(())) => unreachable!("a read that ran answered, catching its own panic"),
        };
        let touched = match (tracked, reads) {
            (Some(tracked), Some(reads)) => tracked.file(reads),
            _ => None,
        };
        CallAnswer { answer, touched }
    }

    /// Tells the listener of the builder's commits, if there is one, that the commit at `sequence` wrote into the reads
    /// tracked under `token`: what `answer_call_read` found of a read filed after that commit. Told once the answer has
    /// been handed over, so that the listener never hears of a change to an answer its caller hasn't got
    pub fn tell_touched(&self, sequence: u64, token: u32) {
        let listener = self.tracks.lock().builder_commit_listener();
        let Some(listener) = listener else { return };
        let heard =
            watch::as_callback(&self.tracks, || catch_unwind(AssertUnwindSafe(|| listener(sequence, vec![token]))));
        // One that panics is heard no more, as when it panics hearing a builder's commit
        if heard.is_err() {
            self.tracks.lock().hear_builder_commits(None).expect("dropping the listener refuses nothing");
        }
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
        let held_txn = store.begin()?;
        place.taken = true;
        run_point!(UNIT_HELD);
        held.validate(&held_txn.txn, store.db())?;
        let schema = Arc::clone(&held);
        drop(held);
        let builders = self.builders()?;
        // SAFETY: `held_txn` borrows `store`'s environment. `Unit` holds `store` in a field declared after `txn`,
        // so `WriteTxn` (dropping its `RwTxn` before `_writer` and `_gate`) drops before `store` and its environment.
        let held_txn: crate::store::WriteTxn<'static> = unsafe { held_txn.into_static() };
        let logging = !self.tracks.lock().is_empty();
        let (tracks, schemas) = (Arc::clone(&self.tracks), self.observing_schema());
        // From here the unit holds the place, and ends it
        place.taken = false;
        drop(place);
        Ok(Unit {
            txn: Some(held_txn),
            state: UnitState::new(logging),
            tracks,
            pending_tracks: Vec::new(),
            schemas,
            poisoned: None,
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
    // Field order is drop order: `WriteTxn` drops its `RwTxn` before `_writer` and `_gate`, and all of them before
    // `store` (which owns the LMDB `Env` that `WriteTxn` borrows).
    txn: Option<crate::store::WriteTxn<'static>>,
    state: UnitState,
    /// Why the unit can't commit: LMDB can't go on with a transaction a write or undo left broken
    poisoned: Option<Error>,
    schema: Arc<EngineSchema>,
    schemas: Arc<RwLock<Arc<EngineSchema>>>,
    store: Arc<Store>,
    tracks: watch::SharedTracks,
    /// Read sets recorded by `Unit::read`, filed on `tracks` only when this unit commits so failed reads and aborted
    /// units never leave uncommitted-write read sets on tokens
    pending_tracks: Vec<(u32, tracking::ReadSet)>,
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
        let key = keys::checkpoint(name);
        let mut t = self.view(0.0)?;
        let depth = t.state.savepoint();
        match t.put(&key, value) {
            Ok(()) => {
                t.release(depth)?;
                Ok(())
            }
            Err(error) => {
                let rolled_back = t.rollback_to(depth);
                if error.code == ErrorCode::MapFull || rolled_back.is_err() {
                    self.poisoned = Some(error.clone());
                }
                Err(error)
            }
        }
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
        let held = self.txn.as_ref().ok_or_else(|| Error::new(ErrorCode::Closed, "The unit has ended"))?;
        self.store.next_sequence(&held.txn)
    }

    fn view(&mut self, now: f64) -> Result<Txn<'_, 'static>> {
        if let Some(error) = &self.poisoned {
            return Err(Error::new(
                error.code,
                format!("The unit can't go on, and must be aborted: {}", error.message),
            ));
        }
        let held = self.txn.as_mut().ok_or_else(|| Error::new(ErrorCode::Closed, "The unit has ended"))?;
        Ok(Txn { txn: &mut held.txn, db: self.store.db(), state: &mut self.state, schema: &self.schema, now })
    }

    /// Applies a batch in the unit, keeping none of it when any operation is refused
    pub fn write(&mut self, batch: &Batch) -> Result<BatchResult> {
        let now = batch.now.unwrap_or_else(now_ms);
        let mut t = self.view(now)?;
        let depth = t.state.savepoint();
        match apply_all(&mut t, batch) {
            Ok(results) => {
                t.release(depth)?;
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
        let t = self.view(0.0)?;
        let Some(token) = reads.track else { return reads::run(&t, &reads.reads) };
        let recording = tracking::Recording::new(&t);
        let answer = reads::run(&recording, &reads.reads)?;
        let read_set = recording.into_reads();
        self.pending_tracks.push((token, read_set));
        Ok(answer)
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
        let mut view = self.view(0.0)?;
        let result = view.rollback_to(depth);
        if let Err(error) = &result
            && error.code != ErrorCode::BatchInvalid
        {
            self.poisoned = Some(error.clone());
        }
        result
    }

    /// Closes the savepoint at `depth`, keeping its writes in the unit around it
    pub fn release(&mut self, depth: usize) -> Result<()> {
        self.view(0.0)?.release(depth)
    }

    /// Commits the unit. `durable` waits for the commit's sync
    pub fn commit(mut self, durable: bool) -> Result<ChangeSet> {
        let checked = self.view(0.0).and_then(|mut t| {
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
        let held = self.txn.take().expect("an open unit has its transaction");
        run_point!(UNIT_COMMITTING);
        self.store.deposit(std::mem::take(&mut self.state.edges));
        self.store.deposit_touched(changed::Touched::of(&self.state.events, &self.schema));
        let committed = held.commit(&self.store);
        self.ended = true;
        self.store.end_unit();
        let sequence = committed?;
        if !self.pending_tracks.is_empty() {
            let mut tracks = self.tracks.lock();
            for (token, reads) in self.pending_tracks.drain(..) {
                tracks.file(token, reads);
            }
        }
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
        // `WriteTxn` drops `txn` first, then `_writer`, then `_gate`. A commit that panicked once it had taken
        // `txn` has not told the store, which is told here as the unit is dropped.
        self.txn = None;
        self.pending_tracks.clear();
        if !self.ended {
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
