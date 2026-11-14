//! Entities and their attributes in the store's keys (`docs/architecture/format.md`, Keys):
//!
//! - `p/i id` → `[seq, trashed, changed]`: that the entity exists, its place in the store's sequence, the trash, and
//!   the sequence of the commit that last wrote it (`changed`), with its place in `d/h` there; the trash
//! - `p/v id field` → the field's value, so an entity's fields are one range and a row is one read
//! - `d/e type seq` → id for a live entity, `d/t type seq` → id for a trashed one: a type's entities in creation order
//! - `d/c type` → the type's count of live entities
//! - `d/f type field` → that the type's entities have held the field: put by the first write, kept when its last value
//!   goes, as a property key is in a graph database, and pruned only by `rebuild`
//! - `d/x type index parts…` → the places of the entities a value index files under those parts (`places`), kept by
//!   every attribute write
//! - `d/xw type index leading…`, `d/xu type index leading…` → the places of the entities a value index files
//!   besides its entries, those without a value in its last field and those it doesn't order, each under what its
//!   fields before the last hold, so that the index accounts for every entity an equality on its other fields can
//!   name (`ValueIndex::accounts`)
//!
//! Every write goes through `put_field` and `delete_field`, which keep the derived keys, the text indexes' (`text`)
//! among them.

use rmpv::Value;
use std::collections::BTreeSet;

use crate::error::{Error, ErrorCode, Result};
use crate::format::Prefix;
use crate::format::key::{Part, decode as decode_key};
use crate::schema::IndexDef;

use super::changes::Event;
use super::keys::{self, ROLE_INDEX, SHORT_CODE_INDEX};
use super::unit::{Read, Txn};
use super::{ids, places, values};

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
    /// The sequence of the commit that last wrote it, 0 before one has
    pub changed: u64,
}

pub fn record(t: &impl Read, id: &str) -> Result<Option<Record>> {
    let Some(bytes) = t.get(&keys::entity(id))? else { return Ok(None) };
    decode_record(id, &bytes).map(Some)
}

/// A record as `p/i` holds it; one written before records kept their change reads as changed at 0
pub fn decode_record(id: &str, bytes: &[u8]) -> Result<Record> {
    let corrupt = || Error::new(ErrorCode::Corrupted, format!("{id}'s record doesn't read"));
    let value = values::decode(bytes)?;
    let items = value.as_array().ok_or_else(corrupt)?;
    let seq = items.first().and_then(Value::as_u64).ok_or_else(corrupt)?;
    let trashed = items.get(1).and_then(Value::as_bool).ok_or_else(corrupt)?;
    let changed = items.get(2).and_then(Value::as_u64).unwrap_or(0);
    Ok(Record { seq, trashed, changed })
}

pub fn encode_record(record: Record) -> Vec<u8> {
    values::encode(&Value::Array(vec![record.seq.into(), record.trashed.into(), record.changed.into()]))
}

fn put_record(t: &mut Txn<'_, '_>, id: &str, record: Record) -> Result<()> {
    t.put(&keys::entity(id), &encode_record(record))
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
    let record = Record { seq, trashed: false, changed: 0 };
    put_record(t, id, record)?;
    let entity_type = ids::type_of(id).to_owned();
    t.put(&keys::of_type(false, &entity_type, seq), &values::encode(&id.into()))?;
    // Its place in its type's change order is put as its commit stamps it (`changed::stamp`)
    add_count(t, &entity_type, 1)?;
    super::ranks::liveness(t, id, true)?;
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
    put_record(t, id, Record { trashed, ..record })?;
    add_count(t, &entity_type, if trashed { -1 } else { 1 })?;
    super::ranks::liveness(t, id, !trashed)?;
    t.event(if trashed { Event::Trashed { id: id.to_owned() } } else { Event::Restored { id: id.to_owned() } });
    Ok(())
}

/// The entity of a type at a place in the store's sequence, live or in the trash, and which: what a value index's
/// place names
pub fn at_place(t: &impl Read, entity_type: &str, place: u64) -> Result<Option<(String, bool)>> {
    for trashed in [false, true] {
        if let Some(id) = t.get(&keys::of_type(trashed, entity_type, place))? {
            return Ok(Some((values::decode(&id)?.as_str().unwrap_or_default().to_owned(), trashed)));
        }
    }
    Ok(None)
}

pub fn get(t: &impl Read, id: &str, field: &str) -> Result<Option<Value>> {
    t.get(&keys::value(id, field))?.map(|b| values::decode_stored(&b)).transpose()
}

/// The entity's fields, in key order, each as its msgpack, decompressed where it is stored compressed
pub fn fields(t: &impl Read, id: &str) -> Result<Vec<(String, Vec<u8>)>> {
    fields_stored(t, id)?.into_iter().map(|(field, bytes)| Ok((field, values::unstored_owned(bytes)?))).collect()
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
    if stored.is_none() {
        register_field(t, id, field)?;
    }
    reindex(t, id, field, before.as_ref(), Some(value), seq, false)?;
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

/// Writes several fields of one entity as one write does, each as `put_field` writes it. A unique index over more than
/// one of them is held to the values the write leaves: checked at each field it would be held to values the entity
/// only passes through, with the fields before written and those after not yet, and refuse a write for values another
/// entity holds that this one never ends on
pub fn put_fields(t: &mut Txn<'_, '_>, id: &str, fields: &[(String, Value)]) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    let written = |index: &ValueIndex| {
        // mutant-skip: replace == with != in put_fields
        // Either counts in an index the write writes one field of, or none. Checked once the write's other fields
        // are in, the first is held to the values of its own that its field's write would have held it to, and the
        // second holds nothing new to check: only which of two refusals a write is given tells them from the code
        index.fields.iter().filter(|f| fields.iter().any(|(name, _)| name == *f)).count()
    };
    let spanning: Vec<ValueIndex> = if t.state.unchecked {
        Vec::new()
    } else {
        // mutant-skip: replace > with >= in put_fields (as above)
        value_indexes(t.schema(), &entity_type).into_iter().filter(|i| i.unique && written(i) > 1).collect()
    };
    let held = |t: &Txn<'_, '_>, index: &ValueIndex| -> Result<Vec<Option<Value>>> {
        index.fields.iter().map(|f| get(t, id, f)).collect()
    };
    let before: Vec<Vec<Option<Value>>> = spanning.iter().map(|index| held(t, index)).collect::<Result<_>>()?;
    t.state.unique_deferred = spanning.iter().map(|index| index.name.clone()).collect();
    let put = fields.iter().try_for_each(|(name, value)| put_field(t, id, name, value));
    // Cleared whatever came of the writes: a unit goes on after a refused batch, its next write checked as every one is
    t.state.unique_deferred.clear();
    put?;
    for (index, before) in spanning.iter().zip(&before) {
        let after = held(t, index)?;
        check_unique(t, &entity_type, index, id, before, &after)?;
    }
    Ok(())
}

/// Registers a field the entity didn't hold, so the write that finds it registered writes nothing a catalog reads
fn register_field(t: &mut Txn<'_, '_>, id: &str, field: &str) -> Result<()> {
    let key = keys::field_name(ids::type_of(id), field);
    if t.get(&key)?.is_none() { t.put(&key, &[]) } else { Ok(()) }
}

/// Removes a field, keeping its type's value indexes and, for `trashedAt`, the trash, reading as `put_field` does
pub fn delete_field(t: &mut Txn<'_, '_>, id: &str, field: &str) -> Result<()> {
    let key = keys::value(id, field);
    let Some(stored) = t.get(&key)? else { return Ok(()) };
    let before = values::decode_stored(&stored)?;
    let seq = place_for(t, id, field)?;
    reindex(t, id, field, Some(&before), None, seq, false)?;
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
    let held = fields_stored(t, id)?;
    for (field, bytes) in &held {
        let before = values::decode_stored(bytes)?;
        reindex(t, id, field, Some(&before), None, seq, true)?;
        t.del_over(&keys::value(id, field), Some(bytes))?;
    }
    if let Some(record) = record(t, id)? {
        let entity_type = ids::type_of(id).to_owned();
        // A field it never held left it among those an index on that field alone holds without a value, which no
        // field's removal above reached
        for index in value_indexes(t.schema(), &entity_type).iter().filter(|index| index.accounts()) {
            if let [only] = index.fields.as_slice()
                && !held.iter().any(|(field, _)| field == only)
            {
                places::unfile(t, Other::Without(Vec::new()).key(&entity_type, &index.name), record.seq)?;
            }
        }
        t.del(&keys::of_type(record.trashed, &entity_type, record.seq))?;
        t.del(&keys::changed(&entity_type, record.changed, record.seq))?;
        if !record.trashed {
            add_count(t, &entity_type, -1)?;
        }
        t.del(&keys::entity(id))?;
        super::ranks::liveness(t, id, false)?;
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

impl ValueIndex {
    /// Whether the index accounts for every entity an equality on each of its fields but the last can name, as a
    /// declared index does: such an entity is in its entries, under what those fields hold and then the boolean, the
    /// number or the string its last field holds, or among those without a value there, or among those holding a
    /// value it doesn't order (`files`). A plan reads those entities in the last field's order from such an index,
    /// which leaves none out; for an index on one field they are every entity of its type. The short code's and the
    /// roles' indexes hold only what their lookups read
    pub fn accounts(&self) -> bool {
        !self.fields.iter().any(|name| name == field::SHORT_CODE || name == field::ROLE)
    }

    /// Where the index files an entity whose fields hold `values`, one for each of the index's fields.
    ///
    /// An index on one field has an entry for the field's value, or for each distinct element of its list; an
    /// index that accounts for its entities also files the entity among those without a value when the field is
    /// missing or null, and among those it doesn't order when it holds a list or a value no index holds.
    ///
    /// An index on several fields files the entity under each tuple its fields before the last name
    /// (`leading_of`), which is each tuple of values that equalities on those fields can ask for: an entry there
    /// when the last field holds a boolean, a number or a string the index holds; among those without a value
    /// when the last is missing or null; among those it doesn't order when the last holds anything else. Under a
    /// tuple it files the entity in one of those places, so what one entity holds decides only how its own tuples are
    /// read.
    ///
    /// A unique index on several fields holds an entity to one tuple of values (`unique_keys`), and that tuple is
    /// its one entry, a null among its parts: so a list before the last, which is no one tuple, leaves the entity
    /// among those the index doesn't order under each tuple the list's elements name
    pub fn files(&self, values: &[Option<Value>]) -> Filed {
        let Some((last, before)) = values.split_last() else { return Filed::default() };
        let mut filed = Filed::default();
        let place = |held: &Option<Value>| match held {
            None | Some(Value::Nil) => Place::Without,
            Some(held) => values::index_part(held).map_or(Place::Unordered, Place::Entry),
        };
        if before.is_empty() {
            filed.entries = values::index_parts(last.as_ref()).into_iter().map(|part| vec![part]).collect();
            match place(last) {
                Place::Without => filed.others.push(Other::Without(Vec::new())),
                Place::Unordered => filed.others.push(Other::Unordered(Vec::new())),
                Place::Entry(_) => {}
            }
        } else {
            if self.unique {
                let held = unique_keys(values).into_iter();
                filed.entries = held.filter_map(|key| key.iter().map(values::index_part).collect()).collect();
            }
            let listed = before.iter().any(|held| matches!(held, Some(Value::Array(_))));
            for mut tuple in leading_of(before) {
                match place(last) {
                    _ if self.unique && listed => filed.others.push(Other::Unordered(tuple)),
                    Place::Without => filed.others.push(Other::Without(tuple)),
                    Place::Unordered => filed.others.push(Other::Unordered(tuple)),
                    // A unique index's one entry is its tuple, filed above
                    Place::Entry(part) => {
                        if !self.unique {
                            tuple.push(part);
                            filed.entries.push(tuple);
                        }
                    }
                }
            }
        }
        if !self.accounts() {
            filed.others.clear();
        }
        filed
    }
}

/// Where an index files an entity (`ValueIndex::files`)
#[derive(Debug, Default, PartialEq)]
pub struct Filed {
    /// The parts of the key each of its entries is filed under, which holds the entity's place
    pub entries: Vec<Vec<Part>>,
    /// Where an index that accounts for its entities files it besides
    pub others: Vec<Other>,
}

impl Filed {
    /// The keys the entity's place is filed under as an entry: each tuple of parts' own
    pub fn entry_keys(&self, entity_type: &str, index: &str) -> BTreeSet<Vec<u8>> {
        self.entries.iter().map(|parts| keys::index_range(entity_type, index, parts)).collect()
    }

    /// The keys the entity's place is filed under besides its entries
    pub fn other_keys(&self, entity_type: &str, index: &str) -> BTreeSet<Vec<u8>> {
        self.others.iter().map(|other| other.key(entity_type, index)).collect()
    }
}

/// Where an index's last field puts an entity under a tuple of the fields before it
enum Place {
    /// In an entry, under the part the index files the field's value as
    Entry(Part),
    /// Among those without a value: the field is missing or null
    Without,
    /// Among those the index doesn't order: the field holds a list or a value no index holds
    Unordered,
}

/// Where an index that accounts for its entities files one besides its entries
#[derive(Debug, Clone, PartialEq)]
pub enum Other {
    /// Among those without a value in the index's last field, under the parts its fields before that one hold
    Without(Vec<Part>),
    /// Among those the index doesn't order, under the parts its fields before the last hold: its last field holds
    /// a list or a value no index holds, or it is a unique index and a field before the last holds a list
    Unordered(Vec<Part>),
}

impl Other {
    /// The key an entity's place is filed under
    pub fn key(&self, entity_type: &str, index: &str) -> Vec<u8> {
        match self {
            Other::Without(leading) => keys::index_others(Prefix::IndexWithout, entity_type, index, leading),
            Other::Unordered(leading) => keys::index_others(Prefix::IndexUnordered, entity_type, index, leading),
        }
    }
}

/// What an index's fields before its last hold, as the tuples of parts that equalities on them can name: one for
/// each combination of their values, a list standing for each of its distinct elements, as it does in an index on
/// one field. A value no equality an index answers can ask for names nothing: a missing field, a null, and a value
/// no index holds (a map, bytes, a timestamp, a string past `MAX_INDEXED_STRING`, a list in a list). So there is no
/// tuple when one of the fields names nothing, an empty list among them
fn leading_of(before: &[Option<Value>]) -> Vec<Vec<Part>> {
    let named = |held: &Value| values::index_part(held).filter(|part| *part != Part::Null);
    let mut tuples: Vec<Vec<Part>> = vec![Vec::new()];
    for held in before {
        let mut parts: Vec<Part> = Vec::new();
        match held {
            None => {}
            Some(Value::Array(items)) => {
                for part in items.iter().filter_map(named) {
                    if !parts.contains(&part) {
                        parts.push(part);
                    }
                }
            }
            Some(held) => parts.extend(named(held)),
        }
        tuples = tuples
            .iter()
            .flat_map(|tuple| parts.iter().map(move |part| tuple.iter().chain([part]).cloned().collect()))
            .collect();
    }
    tuples
}

/// Files an entity just made, at `seq`, among those without a value in each index on one field that accounts for its
/// type's entities and whose field it was given no value of, `valued` saying which it was. The writes of its fields
/// found it filed nowhere, so one that gave it no value, or null, left this to do. An index on several fields files
/// it as its fields before the last are written, and nowhere until then
pub fn file_without(t: &mut Txn<'_, '_>, id: &str, seq: u64, valued: impl Fn(&str) -> bool) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    for index in value_indexes(t.schema(), &entity_type).iter().filter(|index| index.accounts()) {
        if let [only] = index.fields.as_slice()
            && !valued(only)
        {
            places::file(t, Other::Without(Vec::new()).key(&entity_type, &index.name), seq)?;
        }
    }
    Ok(())
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

/// Refuses values an index would file an entity under a key longer than LMDB stores (`VALUE_INVALID`), naming the
/// entity and the index: `own` is the key the values have, and the longest key they make is a block's of their
/// places, which is that and a place
pub fn key_fits(own: &[u8], id: &str, entity_type: &str, index: &str) -> Result<()> {
    let key = keys::place_under(&keys::index_blocks(own), 0);
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

/// What a unique index holds an entity to, given each of the index's fields' values: what its entries file
/// (`ValueIndex::files`), as values, and with it what the index can't file (a string past `MAX_INDEXED_STRING`, a
/// map, a timestamp), which only a walk of the type's rows finds. A single field's value or each element of its list; a compound index's fields together, when
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
                // The entities filed under the values, but for this one
                for place in &places::under(t, &keys::index_range(entity_type, &index.name, &parts))? {
                    holder = at_place(t, entity_type, place)?.map(|(held, _)| held).filter(|held| held != id);
                    if holder.is_some() {
                        break;
                    }
                }
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

/// Refiles the entity at `seq` (none for one not stored) in each of its type's value indexes that covers `field`, as
/// its value goes from `before` to `after`, refusing a value a unique index holds for another entity (`UNIQUE`,
/// `SHORT_CODE_IN_USE`). An entity that is `going`, each of its fields removed as it is destroyed, is filed nowhere
/// after
fn reindex(
    t: &mut Txn<'_, '_>,
    id: &str,
    field: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    seq: Option<u64>,
    going: bool,
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
        if index.unique && !t.state.unchecked && !t.state.unique_deferred.contains(&index.name) {
            check_unique(t, &entity_type, &index, id, &values_before, &values_after)?;
        }
        let was = index.files(&values_before);
        let mut now = index.files(&values_after);
        // An entity that is going is filed nowhere besides its entries, which its fields take with them
        if going {
            now.others.clear();
        }
        // Filed as it was: a field written with what it held, or one that changes nothing the index files
        if was == now {
            continue;
        }
        // The keys in their order, each side's once: an entity under many tuples costs their sorting, not their square.
        // An entity just made is filed nowhere besides its entries until its fields are written (`file_without`),
        // and one being destroyed is taken out as the first of an index's fields goes: taking it out from where it
        // isn't changes nothing
        let (had, has) = (was.entry_keys(&entity_type, &index.name), now.entry_keys(&entity_type, &index.name));
        let (had_others, has_others) =
            (was.other_keys(&entity_type, &index.name), now.other_keys(&entity_type, &index.name));
        for key in had.difference(&has).chain(had_others.difference(&has_others)) {
            places::unfile(t, key.clone(), seq)?;
        }
        for key in has.difference(&had).chain(has_others.difference(&had_others)) {
            // What makes a key too long is a value the index holds
            key_fits(key, id, &entity_type, &index.name)?;
            places::file(t, key.clone(), seq)?;
        }
    }
    Ok(())
}
