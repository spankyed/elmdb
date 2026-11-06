//! What changed since a position (`docs/architecture/engine.md`, Changed since): every entity keeps the sequence of the
//! commit that last wrote it, and the store keeps each type's entities in that order, so a reader asking what changed
//! since a sequence reads a range of what changed and nothing else, however far behind it is.
//!
//! - `p/i id` → `[place, trashed, changed]`: `changed` is the sequence of the commit that last wrote the entity, a
//!   field, its trash state or a relation at either end; 0 for one no commit has stamped
//! - `d/h type changed place` → id: a type's entities in change order, derived from `p/i`
//! - `l/d sequence type id`: an entity a commit destroyed, which leaves no `p/i`; the newest kept as the change log keeps
//!   its records, with what trimming dropped recorded (`m/destroyedFloor`) and each type's newest destroy
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

/// The meta key saying how far tombstones have been trimmed
pub(crate) const DESTROYED_FLOOR: &str = "destroyedFloor";

/// The meta key saying every entity has its `d/h` key, which a database from before this index lacks until it is built
pub(crate) const CHANGE_INDEX: &str = "changeIndex";

/// The entities a commit wrote, which it stamps with its sequence
#[derive(Debug, Default, Clone)]
pub(crate) struct Touched {
    ids: BTreeSet<String>,
    destroyed: BTreeSet<String>,
    /// Every entity the store holds, for a commit that replaced them all
    every: bool,
}

impl Touched {
    /// What these events wrote: the entities they name, a relation's at both its ends
    pub(crate) fn of(events: &[Event]) -> Self {
        let mut touched = Self::default();
        for event in events {
            match event {
                Event::Created { id } | Event::Field { id, .. } | Event::Trashed { id } | Event::Restored { id } => {
                    touched.ids.insert(id.clone());
                }
                Event::Destroyed { id } => {
                    touched.ids.insert(id.clone());
                    touched.destroyed.insert(id.clone());
                }
                Event::Linked { source, target, .. }
                | Event::Unlinked { source, target, .. }
                | Event::RelationChanged { source, target, .. } => {
                    touched.ids.insert(source.clone());
                    touched.ids.insert(target.clone());
                }
            }
        }
        touched
    }

    /// Every entity, for a commit that replaced them all
    pub(crate) fn every() -> Self {
        Self { every: true, ..Self::default() }
    }

    pub(crate) fn merge(&mut self, other: Touched) {
        self.every |= other.every;
        self.ids.extend(other.ids);
        self.destroyed.extend(other.destroyed);
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.every && self.ids.is_empty()
    }
}

/// Stamps what `touched` names with `sequence`, in the transaction about to commit as it: each entity moves to that
/// place in its type's change order, and a destroyed one leaves a tombstone
pub(crate) fn stamp(txn: &mut heed::RwTxn<'_>, db: Db, touched: &Touched, sequence: u64) -> Result<()> {
    let every;
    let ids: Box<dyn Iterator<Item = &String>> = if touched.every {
        every = all_ids(txn, db)?;
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
        db.main.delete(txn, &keys::changed(entity_type, record.changed, record.seq))?;
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

/// Drops the tombstones of commits up to `oldest`, which the change log has dropped too, recording how far that went
pub(crate) fn trim_tombstones(txn: &mut heed::RwTxn<'_>, db: Db, oldest: u64) -> Result<()> {
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

/// Builds every entity's `d/h` key from `p/i`, for a database from before the index, and says it is built
pub(crate) fn ensure_index(txn: &mut heed::RwTxn<'_>, db: Db) -> Result<()> {
    if db.main.get(txn, &meta_key(CHANGE_INDEX))?.is_some() {
        return Ok(());
    }
    for id in all_ids(txn, db)? {
        let Some(bytes) = db.main.get(txn, &keys::entity(&id))?.map(<[u8]>::to_vec) else { continue };
        let record = entities::decode_record(&id, &bytes)?;
        db.main.put(
            txn,
            &keys::changed(ids::type_of(&id), record.changed, record.seq),
            &values::encode(&id.as_str().into()),
        )?;
    }
    db.main.put(txn, &meta_key(CHANGE_INDEX), &[1])?;
    Ok(())
}

/// Whether every entity has its `d/h` key
pub(crate) fn index_is_built(txn: &heed::RoTxn<'_, heed::AnyTls>, db: Db) -> Result<bool> {
    Ok(db.main.get(txn, &meta_key(CHANGE_INDEX))?.is_some())
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
            Some(after) if after.entity_type.as_str() < entity_type => (from.sequence, 0, true),
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
            let number = |part: Option<&Part>| match part {
                Some(Part::Number(n)) => Ok(*n as u64),
                _ => Err(Error::new(ErrorCode::Corrupted, "A change order key doesn't read")),
            };
            let changed = number(parts.get(parts.len() - 2))?;
            let place = number(parts.last())?;
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
        let (Some(Part::Number(sequence)), Some(Part::String(entity_type)), Some(Part::String(id))) =
            (parts.get(parts.len() - 3), parts.get(parts.len() - 2), parts.last())
        else {
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
    let mut complete = index_is_built(txn, db)?;
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
    Ok(Changed { ids: found.into_iter().map(|(_, _, _, id)| id).collect(), destroyed, next, done, complete })
}
