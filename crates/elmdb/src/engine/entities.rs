//! Entities and their attributes in the store's keys (`docs/format.md`, Keys):
//!
//! - `p/i id` → `[seq, trashed]`: that the entity exists, its place in the store's sequence, and the trash
//! - `p/v id field` → the field's value, so an entity's fields are one range and a row is one read
//! - `d/e type seq` → id for a live entity, `d/t type seq` → id for a trashed one: a type's entities in creation order
//! - `d/c type` → the type's count of live entities
//! - `d/x type index parts… seq` → id: each value index, kept by every attribute write
//!
//! Every write goes through `put_field` and `delete_field`, which keep the derived keys.

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
    t.get(&keys::value(id, field))?.map(|b| values::decode(&b)).transpose()
}

/// The entity's fields, in key order, as stored bytes
pub fn fields(t: &impl Read, id: &str) -> Result<Vec<(String, Vec<u8>)>> {
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

/// Writes a field, keeping its type's value indexes and, for `trashedAt`, the trash
pub fn put_field(t: &mut Txn<'_, '_>, id: &str, field: &str, value: &Value) -> Result<()> {
    let before = get(t, id, field)?;
    reindex(t, id, field, before.as_ref(), Some(value))?;
    t.put(&keys::value(id, field), &values::encode(value))?;
    t.event(Event::Field { id: id.to_owned(), field: field.to_owned() });
    if field == field::TRASHED_AT {
        set_trashed(t, id, true)?;
    }
    Ok(())
}

/// Removes a field, keeping its type's value indexes and, for `trashedAt`, the trash
pub fn delete_field(t: &mut Txn<'_, '_>, id: &str, field: &str) -> Result<()> {
    let Some(before) = get(t, id, field)? else { return Ok(()) };
    reindex(t, id, field, Some(&before), None)?;
    t.del(&keys::value(id, field))?;
    t.event(Event::Field { id: id.to_owned(), field: field.to_owned() });
    if field == field::TRASHED_AT {
        set_trashed(t, id, false)?;
    }
    Ok(())
}

/// Removes the entity's fields, their index entries, and its place in its type's order; its relations go first,
/// removed by the relations module
pub fn destroy(t: &mut Txn<'_, '_>, id: &str) -> Result<()> {
    for (field, bytes) in fields(t, id)? {
        let before = values::decode(&bytes)?;
        reindex(t, id, &field, Some(&before), None)?;
        t.del(&keys::value(id, &field))?;
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

/// Refiles the entity in each of its type's value indexes that covers `field`, as its value goes from `before` to
/// `after`, refusing an entry a unique index holds for another entity (`UNIQUE`, `SHORT_CODE_IN_USE`)
fn reindex(t: &mut Txn<'_, '_>, id: &str, field: &str, before: Option<&Value>, after: Option<&Value>) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    let indexes: Vec<ValueIndex> =
        value_indexes(t.schema(), &entity_type).into_iter().filter(|i| i.fields.iter().any(|f| f == field)).collect();
    if indexes.is_empty() {
        return Ok(());
    }
    let Some(seq) = record(t, id)?.map(|r| r.seq) else { return Ok(()) };
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
        let was = entries_of(&values_before);
        let now = entries_of(&values_after);
        for parts in was.iter().filter(|p| !now.contains(p)) {
            t.del(&keys::index_entry(&entity_type, &index.name, parts, seq))?;
        }
        for parts in now.iter().filter(|p| !was.contains(p)) {
            if index.unique && !t.state.unchecked {
                let mut taken = None;
                t.visit(&keys::index_range(&entity_type, &index.name, parts), |_, holder| {
                    taken = values::decode(holder).ok().and_then(|v| v.as_str().map(str::to_owned));
                    taken.as_deref() == Some(id)
                })?;
                if let Some(holder) = taken.filter(|h| h != id) {
                    let (code, what) = if index.name == SHORT_CODE_INDEX {
                        (ErrorCode::ShortCodeInUse, "short code".to_owned())
                    } else {
                        (ErrorCode::Unique, format!("unique index {}.{}", entity_type, index.name))
                    };
                    return Err(Error::new(
                        code,
                        format!("{holder} already holds this value of the {what}, so {id} can't"),
                    ));
                }
            }
            t.put(&keys::index_entry(&entity_type, &index.name, parts, seq), &values::encode(&id.into()))?;
        }
    }
    Ok(())
}
