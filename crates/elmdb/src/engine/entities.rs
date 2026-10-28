//! Entities and their attributes in the store's keys (`docs/architecture/format.md`, Keys):
//!
//! - `p/i id` → `[seq, trashed]`: that the entity exists, its place in the store's sequence, and the trash
//! - `p/v id field` → the field's value, so an entity's fields are one range and a row is one read
//! - `d/e type seq` → id for a live entity, `d/t type seq` → id for a trashed one: a type's entities in creation order
//! - `d/c type` → the type's count of live entities
//! - `d/x type index parts… seq` → id: each value index, kept by every attribute write
//!
//! Every write goes through `put_field` and `delete_field`, which keep the derived keys, the text indexes' (`text`)
//! among them.

use rmpv::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::schema::IndexDef;

use super::changes::Event;
use super::keys::{self, ROLE_INDEX, SHORT_CODE_INDEX};
use super::unit::{Read, Txn};
use super::{ids, values};

/// The engine's own fields, which a caller's write can't name
pub mod field {
    pub const ID: &str = "id";
    pub const ENTITY_TYPE: &str = "entityType";
    pub const SHORT_CODE: &str = "shortCode";
    pub const CREATED_AT: &str = "createdAt";
    pub const UPDATED_AT: &str = "updatedAt";
    pub const TRASHED_AT: &str = "trashedAt";
    pub const ROLE: &str = "role";
    /// Not the engine's own: the field `takeLabel` numbers
    pub const LABEL: &str = "label";

    /// The fields only the engine writes
    pub const RESERVED: [&str; 7] = [ID, ENTITY_TYPE, SHORT_CODE, CREATED_AT, UPDATED_AT, TRASHED_AT, ROLE];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    pub trashed: bool,
}

pub fn record(t: &impl Read, id: &str) -> Result<Option<Record>> {
    let Some(bytes) = t.get(&keys::entity(id))? else { return Ok(None) };
    let corrupt = || Error::new(ErrorCode::Corrupted, format!("{id}'s record doesn't read"));
    let value = values::decode(&bytes)?;
    let items = value.as_array().ok_or_else(corrupt)?;
    let seq = items.first().and_then(Value::as_u64).ok_or_else(corrupt)?;
    let trashed = items.get(1).and_then(Value::as_bool).ok_or_else(corrupt)?;
    Ok(Some(Record { seq, trashed }))
}

fn put_record(t: &mut Txn<'_, '_>, id: &str, record: Record) -> Result<()> {
    t.put(&keys::entity(id), &values::encode(&Value::Array(vec![record.seq.into(), record.trashed.into()])))
}

/// The store's sequence, which places entities and relations in the order they were made
pub fn next_seq(t: &mut Txn<'_, '_>) -> Result<u64> {
    let key = keys::sequence("seq");
    let next = match t.get(&key)? {
        Some(bytes) => values::decode(&bytes)?.as_u64().unwrap_or(0) + 1,
        None => 1,
    };
    t.put(&key, &values::encode(&next.into()))?;
    Ok(next)
}

fn add_count(t: &mut Txn<'_, '_>, entity_type: &str, by: i64) -> Result<()> {
    let key = keys::count(entity_type);
    let now = match t.get(&key)? {
        Some(bytes) => values::decode(&bytes)?.as_i64().unwrap_or(0),
        None => 0,
    } + by;
    if now > 0 { t.put(&key, &values::encode(&now.into())) } else { t.del(&key) }
}

pub fn count(t: &impl Read, entity_type: &str) -> Result<u64> {
    Ok(match t.get(&keys::count(entity_type))? {
        Some(bytes) => values::decode(&bytes)?.as_u64().unwrap_or(0),
        None => 0,
    })
}

/// Puts a new entity in the store, last in its type's order
pub fn insert(t: &mut Txn<'_, '_>, id: &str) -> Result<Record> {
    let seq = next_seq(t)?;
    let record = Record { seq, trashed: false };
    put_record(t, id, record)?;
    let entity_type = ids::type_of(id).to_owned();
    t.put(&keys::of_type(false, &entity_type, seq), &values::encode(&id.into()))?;
    add_count(t, &entity_type, 1)?;
    t.event(Event::Created { id: id.to_owned() });
    Ok(record)
}

/// Moves the entity between its type's live and trashed ranges
fn set_trashed(t: &mut Txn<'_, '_>, id: &str, trashed: bool) -> Result<()> {
    let Some(record) = record(t, id)? else { return Ok(()) };
    if record.trashed == trashed {
        return Ok(());
    }
    let entity_type = ids::type_of(id).to_owned();
    t.del(&keys::of_type(record.trashed, &entity_type, record.seq))?;
    t.put(&keys::of_type(trashed, &entity_type, record.seq), &values::encode(&id.into()))?;
    put_record(t, id, Record { seq: record.seq, trashed })?;
    add_count(t, &entity_type, if trashed { -1 } else { 1 })?;
    t.event(if trashed { Event::Trashed { id: id.to_owned() } } else { Event::Restored { id: id.to_owned() } });
    Ok(())
}

pub fn get(t: &impl Read, id: &str, field: &str) -> Result<Option<Value>> {
    t.get(&keys::value(id, field))?.map(|b| values::decode_stored(&b)).transpose()
}

/// The entity's fields, in key order, each as its msgpack, decompressed where it is stored compressed
pub fn fields(t: &impl Read, id: &str) -> Result<Vec<(String, Vec<u8>)>> {
    fields_stored(t, id)?
        .into_iter()
        .map(|(field, bytes)| Ok((field, values::unstored(&bytes)?.into_owned())))
        .collect()
}

/// The entity's fields, in key order, as their stored bytes: what a copy of them or an undo of their removal keeps
pub fn fields_stored(t: &impl Read, id: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let prefix = keys::values_of(id);
    t.scan(&prefix)?
        .into_iter()
        .map(|(k, v)| {
            let field = match decode_key(&k)?.pop() {
                Some(Part::String(f)) => f,
                _ => return Err(Error::new(ErrorCode::Corrupted, format!("A key of {id}'s fields doesn't read"))),
            };
            Ok((field, v))
        })
        .collect()
}

/// The entity's place, for the indexes of its type that cover `field`: read once for all of them, and not at all when
/// none does
fn place_for(t: &Txn<'_, '_>, id: &str, field: &str) -> Result<Option<u64>> {
    let entity_type = ids::type_of(id);
    let covered = value_indexes(t.schema(), entity_type).iter().any(|i| i.fields.iter().any(|f| f == field))
        || super::text::text_indexes(t.schema, entity_type)
            .iter()
            .any(|i| i.fields.iter().any(|(name, _)| name == field))
        || super::vector::vector_indexes(t.schema, entity_type).iter().any(|i| i.from.iter().any(|name| name == field))
        || super::trigram::trigram_indexes(t.schema, entity_type).iter().any(|i| i.fields.iter().any(|f| f == field));
    if !covered {
        return Ok(None);
    }
    Ok(record(t, id)?.map(|r| r.seq))
}

/// Writes a field, keeping its type's value indexes and, for `trashedAt`, the trash. What the field held is read once,
/// for the indexes and the unit's undo log both, and the entity's place once for all its indexes
pub fn put_field(t: &mut Txn<'_, '_>, id: &str, field: &str, value: &Value) -> Result<()> {
    let key = keys::value(id, field);
    let stored = t.get(&key)?;
    let before = stored.as_deref().map(values::decode_stored).transpose()?;
    let seq = place_for(t, id, field)?;
    reindex(t, id, field, before.as_ref(), Some(value), seq)?;
    t.put_over(&key, &values::stored(value), stored.as_deref())?;
    super::text::refile(t, id, field, seq, Some(value))?;
    super::vector::refile(t, id, field, seq, Some(value))?;
    super::trigram::refile(t, id, field, seq, before.as_ref(), Some(value))?;
    t.event(Event::Field { id: id.to_owned(), field: field.to_owned() });
    if field == field::TRASHED_AT {
        set_trashed(t, id, true)?;
    }
    Ok(())
}

/// Removes a field, keeping its type's value indexes and, for `trashedAt`, the trash, reading as `put_field` does
pub fn delete_field(t: &mut Txn<'_, '_>, id: &str, field: &str) -> Result<()> {
    let key = keys::value(id, field);
    let Some(stored) = t.get(&key)? else { return Ok(()) };
    let before = values::decode_stored(&stored)?;
    let seq = place_for(t, id, field)?;
    reindex(t, id, field, Some(&before), None, seq)?;
    t.del_over(&key, Some(&stored))?;
    super::text::refile(t, id, field, seq, None)?;
    super::vector::refile(t, id, field, seq, None)?;
    super::trigram::refile(t, id, field, seq, Some(&before), None)?;
    t.event(Event::Field { id: id.to_owned(), field: field.to_owned() });
    if field == field::TRASHED_AT {
        set_trashed(t, id, false)?;
    }
    Ok(())
}

/// Removes the entity's fields, their index entries, and its place in its type's order; its relations go first,
/// removed by the relations module
pub fn destroy(t: &mut Txn<'_, '_>, id: &str) -> Result<()> {
    super::text::remove(t, id)?;
    super::vector::remove(t, id)?;
    super::trigram::remove(t, id)?;
    let seq = record(t, id)?.map(|r| r.seq);
    for (field, bytes) in fields_stored(t, id)? {
        let before = values::decode_stored(&bytes)?;
        reindex(t, id, &field, Some(&before), None, seq)?;
        t.del_over(&keys::value(id, &field), Some(&bytes))?;
    }
    if let Some(record) = record(t, id)? {
        let entity_type = ids::type_of(id).to_owned();
        t.del(&keys::of_type(record.trashed, &entity_type, record.seq))?;
        if !record.trashed {
            add_count(t, &entity_type, -1)?;
        }
        t.del(&keys::entity(id))?;
        t.event(Event::Destroyed { id: id.to_owned() });
    }
    Ok(())
}

/// A value index on a type, as the writes keep it
pub struct ValueIndex {
    pub name: String,
    pub fields: Vec<String>,
    pub unique: bool,
}

/// The type's value indexes: its declared ones, and the short code's and the roles' every type has
pub fn value_indexes(schema: &crate::schema::Schema, entity_type: &str) -> Vec<ValueIndex> {
    let mut out = vec![
        ValueIndex { name: SHORT_CODE_INDEX.to_owned(), fields: vec![field::SHORT_CODE.to_owned()], unique: true },
        ValueIndex { name: ROLE_INDEX.to_owned(), fields: vec![field::ROLE.to_owned()], unique: false },
    ];
    if let Some(def) = schema.types.get(entity_type) {
        for (name, index) in &def.indexes {
            if let IndexDef::Value { fields, unique } = index {
                out.push(ValueIndex { name: name.clone(), fields: fields.clone(), unique: *unique });
            }
        }
    }
    out
}

/// The parts tuples an index files an entity under, given each of the index's fields' values: a single field's value or
/// each element of its list; a compound index's fields, each a scalar, together, or nothing when any is missing
pub fn entries_of(fields: &[Option<Value>]) -> Vec<Vec<Part>> {
    if let [only] = fields {
        return values::index_parts(only.as_ref()).into_iter().map(|p| vec![p]).collect();
    }
    let mut tuple = Vec::new();
    for value in fields {
        match value.as_ref().and_then(values::index_part) {
            Some(part) => tuple.push(part),
            None => return Vec::new(),
        }
    }
    vec![tuple]
}

/// Refuses an index entry whose key is longer than LMDB stores (`VALUE_INVALID`), naming the entity and the index
pub fn key_fits(key: &[u8], id: &str, entity_type: &str, index: &str) -> Result<()> {
    if key.len() <= values::MAX_KEY_BYTES {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::ValueInvalid,
        format!(
            "{id}'s entry in index {entity_type}.{index} would be a {}-byte key, and a key is at most {} bytes: \
             shorten the values the index holds",
            key.len(),
            values::MAX_KEY_BYTES
        ),
    ))
}

/// What a unique index holds an entity to, given each of the index's fields' values: what `entries_of` files, as values,
/// and with it what the index can't file (a string past `MAX_INDEXED_STRING`, a map, a timestamp), which only a walk of
/// the type's rows finds. A single field's value or each element of its list; a compound index's fields together, when
/// each is present and none is a list
pub(super) fn unique_keys(fields: &[Option<Value>]) -> Vec<Vec<Value>> {
    let mut keys: Vec<Vec<Value>> = Vec::new();
    let mut push = |key: Vec<Value>| {
        if !keys.iter().any(|k| same_key(k, &key)) {
            keys.push(key);
        }
    };
    match fields {
        [None] => {}
        [Some(Value::Array(items))] => items.iter().for_each(|v| push(vec![v.clone()])),
        [Some(v)] => push(vec![v.clone()]),
        _ => {
            let tuple: Option<Vec<Value>> =
                fields.iter().map(|v| v.clone().filter(|v| !matches!(v, Value::Array(_)))).collect();
            if let Some(tuple) = tuple {
                push(tuple);
            }
        }
    }
    keys
}

fn same_key(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values::same(x, y))
}

/// The other entity of the type, live or trashed, that a walk of its rows finds holding one of `wanted` in `index`
fn holder_by_walk(
    t: &impl Read,
    entity_type: &str,
    index: &ValueIndex,
    id: &str,
    wanted: &[Vec<Value>],
) -> Result<Option<String>> {
    for trashed in [false, true] {
        for (_, other) in t.scan(&keys::type_range(trashed, entity_type))? {
            let other = values::decode(&other)?.as_str().unwrap_or_default().to_owned();
            if other == id {
                continue;
            }
            let fields: Vec<Option<Value>> = index.fields.iter().map(|f| get(t, &other, f)).collect::<Result<_>>()?;
            if unique_keys(&fields).iter().any(|k| wanted.iter().any(|w| same_key(k, w))) {
                return Ok(Some(other));
            }
        }
    }
    Ok(None)
}

/// Refuses values of a unique index that another entity of the type holds: read from the index when it is built and
/// can hold them, and otherwise found by walking the type's rows, so the index is unique from the moment it is
/// declared and for values it can't file
fn check_unique(
    t: &Txn<'_, '_>,
    entity_type: &str,
    index: &ValueIndex,
    id: &str,
    before: &[Option<Value>],
    after: &[Option<Value>],
) -> Result<()> {
    let was = unique_keys(before);
    let fresh: Vec<Vec<Value>> =
        unique_keys(after).into_iter().filter(|k| !was.iter().any(|w| same_key(w, k))).collect();
    if fresh.is_empty() {
        return Ok(());
    }
    let built = super::derive::is_built(t, entity_type, &index.name)?;
    let mut walk = Vec::new();
    let mut holder = None;
    for key in fresh {
        let parts: Option<Vec<Part>> = key.iter().map(values::index_part).collect();
        match parts.filter(|_| built) {
            Some(parts) => {
                t.visit(&keys::index_range(entity_type, &index.name, &parts), |_, held| {
                    holder = values::decode(held).ok().and_then(|v| v.as_str().map(str::to_owned));
                    holder.as_deref() == Some(id)
                })?;
                holder = holder.filter(|h| h != id);
                if holder.is_some() {
                    break;
                }
            }
            None => walk.push(key),
        }
    }
    if holder.is_none() && !walk.is_empty() {
        holder = holder_by_walk(t, entity_type, index, id, &walk)?;
    }
    let Some(holder) = holder else { return Ok(()) };
    let (code, what) = if index.name == SHORT_CODE_INDEX {
        (ErrorCode::ShortCodeInUse, "short code".to_owned())
    } else {
        (ErrorCode::Unique, format!("unique index {entity_type}.{}", index.name))
    };
    Err(Error::new(code, format!("{holder} already holds this value of the {what}, so {id} can't")))
}

/// Refiles the entity in each of its type's value indexes that covers `field`, as its value goes from `before` to
/// `after`, refusing a value a unique index holds for another entity (`UNIQUE`, `SHORT_CODE_IN_USE`)
/// Keeps the value indexes covering `field` of the entity at `seq` (none for one not stored) as it goes from `before`
/// to `after`
fn reindex(
    t: &mut Txn<'_, '_>,
    id: &str,
    field: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    seq: Option<u64>,
) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    let indexes: Vec<ValueIndex> =
        value_indexes(t.schema(), &entity_type).into_iter().filter(|i| i.fields.iter().any(|f| f == field)).collect();
    if indexes.is_empty() {
        return Ok(());
    }
    let Some(seq) = seq else { return Ok(()) };
    for index in indexes {
        let mut values_before = Vec::new();
        let mut values_after = Vec::new();
        for f in &index.fields {
            if f == field {
                values_before.push(before.cloned());
                values_after.push(after.cloned());
            } else {
                let other = get(t, id, f)?;
                values_before.push(other.clone());
                values_after.push(other);
            }
        }
        if index.unique && !t.state.unchecked {
            check_unique(t, &entity_type, &index, id, &values_before, &values_after)?;
        }
        let was = entries_of(&values_before);
        let now = entries_of(&values_after);
        for parts in was.iter().filter(|p| !now.contains(p)) {
            t.del(&keys::index_entry(&entity_type, &index.name, parts, seq))?;
        }
        for parts in now.iter().filter(|p| !was.contains(p)) {
            let key = keys::index_entry(&entity_type, &index.name, parts, seq);
            key_fits(&key, id, &entity_type, &index.name)?;
            t.put(&key, &values::encode(&id.into()))?;
        }
    }
    Ok(())
}
