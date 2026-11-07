//! What changed since a position (`docs/architecture/engine.md`, Changed since): every entity keeps the sequence of the
//! commit that last wrote it, and the store keeps each type's entities in that order, so a reader asking what changed
//! since a sequence reads a range of what changed and nothing else, however far behind it is.
//!
//! - `p/i id` → `[place, trashed, changed]`: `changed` is the sequence of the commit that last wrote the entity, a
//!   field, its trash state or a relation at either end; 0 for one no commit has stamped
//! - `d/h type changed place` → id: a type's entities in change order, derived from `p/i`
//! - `l/d sequence type id`: an entity a commit destroyed, which leaves no `p/i`; those of the last `TOMBSTONE_COMMITS`
//!   commits kept, with what trimming dropped recorded (`m/destroyedFloor`) and each type's newest destroy
//!   (`m/destroyed:<type>`), so a reader that may have missed one is told so
//!
//! A commit stamps what it wrote once, as it commits (`Store::commit_txn`), whatever number of fields it wrote.

use std::collections::BTreeSet;
use std::ops::Bound;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::store::{Db, meta_key};

use super::changes::Event;
use super::{entities, ids, keys, values};

/// The meta key a type's newest destroy is under
fn destroyed_meta(entity_type: &str) -> Vec<u8> {
    meta_key(&format!("destroyed:{entity_type}"))
}

/// How many commits' tombstones are kept. A reader further behind is told it may have missed a destroy, and starts again
pub const TOMBSTONE_COMMITS: u64 = 4096;

/// How often a commit also drops the tombstones that have fallen out of reach, which may run over by this many until then.
/// A delete copies the pages down to the oldest key, so deleting one per commit would add a second path to every commit:
/// trimming in batches pays that once in a while
pub const TRIM_EVERY: u64 = 64;

/// The meta key saying how far tombstones have been trimmed
pub(crate) const DESTROYED_FLOOR: &str = "destroyedFloor";

/// The meta key under which a type keeping a change order says from which sequence it has: built over the entities it
/// held then, so a reader from before that can't know what changed before it
fn built_meta(entity_type: &str) -> Vec<u8> {
    meta_key(&format!("changeOrder:{entity_type}"))
}

/// Whether the schema has the type keep a change order
pub(crate) fn kept(schema: &crate::schema::Schema, entity_type: &str) -> bool {
    schema.types.get(entity_type).is_some_and(|def| def.change_order)
}

/// The entities a commit wrote, which it stamps with its sequence
#[derive(Debug, Default, Clone)]
pub(crate) struct Touched {
    ids: BTreeSet<String>,
    /// Those created by the commit, which have no place in the change order yet
    created: BTreeSet<String>,
    destroyed: BTreeSet<String>,
    /// Every entity of `types`, for a commit that replaced them all
    every: bool,
    types: BTreeSet<String>,
}

impl Touched {
    /// What these events wrote of the types that keep a change order: the entities they name, a relation's at both its
    /// ends
    pub(crate) fn of(events: &[Event], schema: &crate::schema::Schema) -> Self {
        let mut touched = Self::default();
        let mut add = |id: &String, event: Option<&Event>| {
            if kept(schema, ids::type_of(id)) {
                touched.ids.insert(id.clone());
                match event {
                    // mutant-skip: delete match arm Some\(Event::Created\{..\}\) in Touched::of
                    // Spares the delete of a place a new entity never had: without it the delete finds nothing, and the
                    // order is the same
                    Some(Event::Created { .. }) => {
                        touched.created.insert(id.clone());
                    }
                    Some(Event::Destroyed { .. }) => {
                        touched.created.remove(id);
                        touched.destroyed.insert(id.clone());
                    }
                    _ => {}
                }
            }
        };
        for event in events {
            match event {
                Event::Created { id } | Event::Destroyed { id } => add(id, Some(event)),
                Event::Field { id, .. } | Event::Trashed { id } | Event::Restored { id } => add(id, None),
                Event::Linked { source, target, .. }
                | Event::Unlinked { source, target, .. }
                | Event::RelationChanged { source, target, .. } => {
                    add(source, None);
                    add(target, None);
                }
            }
        }
        touched
    }

    /// What `other` wrote as well
    pub(crate) fn merge(&mut self, other: Touched) {
        // mutant-skip: replace Touched::merge with \(\)
        // Two deposits meet only in a replacement, whose first is every entity of every type that keeps an order, so
        // what a merge adds to it is in it already
        self.every |= other.every;
        self.types.extend(other.types);
        self.ids.extend(other.ids);
        self.created.extend(other.created);
        self.destroyed.extend(other.destroyed);
    }

    /// Every entity of the types that keep a change order, for a commit that replaced them all
    pub(crate) fn every(schema: &crate::schema::Schema) -> Self {
        let types = schema.types.iter().filter(|(_, def)| def.change_order).map(|(name, _)| name.clone()).collect();
        Self { every: true, types, ..Self::default() }
    }
}

/// Stamps what `touched` names with `sequence`, in the transaction about to commit as it: each entity moves to that
/// place in its type's change order, and a destroyed one leaves a tombstone
pub(crate) fn stamp(txn: &mut heed::RwTxn<'_>, db: Db, touched: &Touched, sequence: u64) -> Result<()> {
    let every;
    let ids: Box<dyn Iterator<Item = &String>> = if touched.every {
        every = all_ids(txn, db)?.into_iter().filter(|id| touched.types.contains(ids::type_of(id))).collect::<Vec<_>>();
        Box::new(every.iter())
    } else {
        Box::new(touched.ids.iter())
    };
    for id in ids {
        let entity_type = ids::type_of(id);
        let Some(bytes) = db.main.get(txn, &keys::entity(id))?.map(<[u8]>::to_vec) else {
            if touched.destroyed.contains(id) {
                db.main.put(txn, &keys::destroyed(sequence, entity_type, id), &[])?;
                db.main.put(txn, &destroyed_meta(entity_type), &sequence.to_be_bytes())?;
            }
            continue;
        };
        let record = entities::decode_record(id, &bytes)?;
        if record.changed == sequence {
            continue;
        }
        if !touched.created.contains(id) {
            db.main.delete(txn, &keys::changed(entity_type, record.changed, record.seq))?;
        }
        db.main.put(txn, &keys::changed(entity_type, sequence, record.seq), &values::encode(&id.as_str().into()))?;
        db.main.put(
            txn,
            &keys::entity(id),
            &entities::encode_record(entities::Record { changed: sequence, ..record }),
        )?;
    }
    Ok(())
}

fn all_ids(txn: &heed::RoTxn<'_, heed::AnyTls>, db: Db) -> Result<Vec<String>> {
    let prefix = keys::all_entities();
    let mut out = Vec::new();
    for item in db.main.prefix_iter(txn, &prefix)? {
        let (key, _) = item?;
        let Some(Part::String(id)) = decode_key(key)?.pop() else {
            return Err(Error::new(ErrorCode::Corrupted, "An entity's key doesn't read"));
        };
        out.push(id);
    }
    Ok(out)
}

/// At the commit with `sequence`, every `TRIM_EVERY` commits: drops the tombstones of commits up to `TOMBSTONE_COMMITS`
/// before it, recording how far that went
pub(crate) fn trim_tombstones(txn: &mut heed::RwTxn<'_>, db: Db, sequence: u64) -> Result<()> {
    if !sequence.is_multiple_of(TRIM_EVERY) {
        return Ok(());
    }
    let Some(oldest) = sequence.checked_sub(TOMBSTONE_COMMITS) else { return Ok(()) };
    let prefix = keys::destroyed_prefix();
    let reach = keys::destroyed_through(oldest);
    let stale: Vec<Vec<u8>> = db
        .main
        .range(txn, &(Bound::Included(prefix.as_slice()), Bound::Excluded(reach.as_slice())))?
        .map(|item| item.map(|(key, _)| key.to_vec()))
        .collect::<std::result::Result<_, _>>()?;
    if stale.is_empty() {
        return Ok(());
    }
    for key in stale {
        db.main.delete(txn, &key)?;
    }
    db.main.put(txn, &meta_key(DESTROYED_FLOOR), &oldest.to_be_bytes())?;
    Ok(())
}

/// Brings each type's change order into line with the schema: built, from the entities it holds, for a type that keeps
/// one and has none, which names the sequence it has it from; dropped for a type that no longer keeps one
pub(crate) fn settle(txn: &mut heed::RwTxn<'_>, db: Db, schema: &crate::schema::Schema) -> Result<()> {
    let sequence = db
        .get(txn, &meta_key(crate::format::meta::SEQUENCE))?
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map_or(0, u64::from_be_bytes);
    for (entity_type, def) in &schema.types {
        let built = db.main.get(txn, &built_meta(entity_type))?.is_some();
        if def.change_order && !built {
            for trashed in [false, true] {
                let entries: Vec<(Vec<u8>, Vec<u8>)> = db
                    .main
                    .prefix_iter(txn, &keys::type_range(trashed, entity_type))?
                    .map(|item| item.map(|(key, value)| (key.to_vec(), value.to_vec())))
                    .collect::<std::result::Result<_, _>>()?;
                for (_, value) in entries {
                    let Some(id) = values::decode(&value)?.as_str().map(str::to_owned) else { continue };
                    let Some(bytes) = db.main.get(txn, &keys::entity(&id))?.map(<[u8]>::to_vec) else { continue };
                    let record = entities::decode_record(&id, &bytes)?;
                    db.main.put(
                        txn,
                        &keys::changed(entity_type, record.changed, record.seq),
                        &values::encode(&id.as_str().into()),
                    )?;
                }
            }
            db.main.put(txn, &built_meta(entity_type), &sequence.to_be_bytes())?;
        }
    }
    // A type no longer kept, or no longer declared: its order goes, so check() finds nothing it doesn't derive
    let prefix = {
        let mut key = crate::format::Prefix::Meta.key();
        key.string_prefix("changeOrder:");
        key.to_vec()
    };
    let held: Vec<String> = db
        .main
        .prefix_iter(txn, &prefix)?
        .map(|item| {
            let (key, _) = item?;
            match decode_key(key)?.pop() {
                Some(Part::String(name)) => Ok(name.trim_start_matches("changeOrder:").to_owned()),
                _ => Err(Error::new(ErrorCode::Corrupted, "A change order's meta key doesn't read")),
            }
        })
        .collect::<Result<_>>()?;
    for entity_type in held.iter().filter(|name| !kept(schema, name)) {
        let order: Vec<Vec<u8>> = db
            .main
            .prefix_iter(txn, &keys::changed_of_type(entity_type))?
            .map(|item| item.map(|(key, _)| key.to_vec()))
            .collect::<std::result::Result<_, _>>()?;
        for key in order {
            db.main.delete(txn, &key)?;
        }
        db.main.delete(txn, &built_meta(entity_type))?;
    }
    Ok(())
}

/// Whether some type keeping a change order hasn't built it, or one no longer kept still has one: what `settle` does
pub(crate) fn needs_settling(
    txn: &heed::RoTxn<'_, heed::AnyTls>,
    db: Db,
    schema: &crate::schema::Schema,
) -> Result<bool> {
    for (entity_type, def) in &schema.types {
        if def.change_order != db.main.get(txn, &built_meta(entity_type))?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Where a reader of what changed has read to: everything at or before `sequence`, and at `sequence` itself the
/// entities up to `after` in change order when a page ended within one commit's
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Position {
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<After>,
}

/// The last entity a page read within a commit
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct After {
    #[serde(rename = "type")]
    pub entity_type: String,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub place: u64,
}

/// What changed of some types after a position
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Changed {
    /// The entities that changed and are still there, in change order
    pub ids: Vec<String>,
    /// The sequence of the commit that last changed each of `ids`, in the same order: a reader that wrote some of them
    /// itself can tell its own commits' from another's
    #[cfg_attr(feature = "codegen", ts(type = "number[]"))]
    pub sequences: Vec<u64>,
    /// The entities destroyed since, which nothing else names
    pub destroyed: Vec<String>,
    /// Where to read from next
    pub next: Position,
    /// Whether this is everything there is to read: when it is, `next` is the last commit's sequence
    pub done: bool,
    /// False when what changed can't all be known: a destroy whose tombstone was trimmed may have been missed, or the
    /// index isn't built. The reader starts again from everything
    pub complete: bool,
}

/// What changed of `types` after `from`, at most `limit` entities
pub(crate) fn since(
    txn: &heed::RoTxn<'_, heed::AnyTls>,
    db: Db,
    schema: &crate::schema::Schema,
    types: &[String],
    from: &Position,
    limit: usize,
) -> Result<Changed> {
    if limit == 0 {
        return Err(Error::new(ErrorCode::BatchInvalid, "A read of what changed asks for at least one entity"));
    }
    let last = db
        .get(txn, &meta_key(crate::format::meta::SEQUENCE))?
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map_or(0, u64::from_be_bytes);
    let types: BTreeSet<&str> = types.iter().map(String::as_str).collect();
    let mut found: Vec<(u64, &str, u64, String)> = Vec::new();
    for &entity_type in &types {
        let (changed, place, inclusive) = match &from.after {
            Some(after) if after.entity_type.as_str() == entity_type => (from.sequence, after.place, false),
            // mutant-skip: replace < with <= in since
            // A type equal to the one the reader stopped in took the arm above
            Some(after) if after.entity_type.as_str() < entity_type => (from.sequence, 0, true),
            // A reader at the start has read nothing, so it reads the entities no commit has stamped too. One with a place
            // past this type at 0 has read them
            None if from.sequence == 0 => (0, 0, true),
            _ => (from.sequence.saturating_add(1), 0, true),
        };
        let start = keys::changed(entity_type, changed, place);
        let end = keys::changed_of_type(entity_type);
        let low = if inclusive { Bound::Included(start.as_slice()) } else { Bound::Excluded(start.as_slice()) };
        let before = found.len();
        for item in db.main.range(txn, &(low, Bound::Unbounded))? {
            let (key, value) = item?;
            if !key.starts_with(&end) || found.len() - before == limit {
                break;
            }
            let parts = decode_key(key)?;
            let [.., Part::Number(changed), Part::Number(place)] = parts.as_slice() else {
                return Err(Error::new(ErrorCode::Corrupted, "A change order key doesn't read"));
            };
            let (changed, place) = (*changed as u64, *place as u64);
            let id = values::decode(value)?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::new(ErrorCode::Corrupted, "A change order entry doesn't name its entity"))?;
            found.push((changed, entity_type, place, id));
        }
    }
    found.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    // Fewer than a page between them is everything; a full page may have more behind it
    let done = found.len() < limit;
    found.truncate(limit);
    let next = match (done, found.last()) {
        (false, Some((changed, entity_type, place, _))) => Position {
            sequence: *changed,
            after: Some(After { entity_type: (*entity_type).to_owned(), place: *place }),
        },
        _ => Position { sequence: last.max(from.sequence), after: None },
    };
    let through = next.sequence;
    let mut destroyed = Vec::new();
    let low = keys::destroyed_through(from.sequence.saturating_add(1));
    for item in db.main.range(txn, &(Bound::Included(low.as_slice()), Bound::Unbounded))? {
        let (key, _) = item?;
        if !key.starts_with(&keys::destroyed_prefix()) {
            break;
        }
        let parts = decode_key(key)?;
        let [.., Part::Number(sequence), Part::String(entity_type), Part::String(id)] = parts.as_slice() else {
            return Err(Error::new(ErrorCode::Corrupted, "A tombstone's key doesn't read"));
        };
        if *sequence as u64 > through {
            break;
        }
        if types.contains(entity_type.as_str()) {
            destroyed.push(id.clone());
        }
    }
    let floor = db
        .get(txn, &meta_key(DESTROYED_FLOOR))?
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map_or(0, u64::from_be_bytes);
    // A type that keeps no order, or keeps one built after the reader's position, can't say what changed before
    let mut complete = true;
    for &entity_type in &types {
        let built = db
            .main
            .get(txn, &built_meta(entity_type))?
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_be_bytes);
        if !kept(schema, entity_type) || built.is_none_or(|built| from.sequence < built) {
            complete = false;
        }
    }
    if from.sequence < floor {
        for &entity_type in &types {
            let newest = db
                .get(txn, &destroyed_meta(entity_type))?
                .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                .map_or(0, u64::from_be_bytes);
            if newest > from.sequence {
                complete = false;
            }
        }
    }
    let sequences = found.iter().map(|(changed, ..)| *changed).collect();
    Ok(Changed { ids: found.into_iter().map(|(_, _, _, id)| id).collect(), sequences, destroyed, next, done, complete })
}
