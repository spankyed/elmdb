//! Derived data from primary data alone (`docs/architecture/format.md`, Keys): what every derived key should hold, which `check()`
//! compares with what is stored and `rebuild()` writes in its place. An index is read only once built (its `d/xb`
//! marker); writes keep every index whether built or not, so building one fills in what predates it.

mod audit;
pub(super) mod sorted;
#[cfg(test)]
use audit::{PrimaryGraph, audit_cycles};

use std::collections::{BTreeMap, BTreeSet};

use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};
use crate::format::Prefix;
use crate::format::key::{Part, decode as decode_key};
use crate::schema::Schema;

use super::entities::{self, ValueIndex, value_indexes};
use super::keys;
use super::places;
use super::unit::{Read, Txn};
use super::{ids, relations, text, trigram, values, vector};

/// One entity as primary data holds it
struct Entity {
    id: String,
    seq: u64,
    trashed: bool,
    changed: u64,
}

/// Each row the schema's rules refuse, read from primary data alone: what `check()` reports of rows, for an operation
/// that holds what it leaves to them before its derived data is rebuilt
pub(super) fn rule_violations(r: &impl Read) -> Result<Vec<Violation>> {
    audit::violations(r)
}

/// Every entity the store holds, from `p/i`
fn entities_of(r: &impl Read) -> Result<Vec<Entity>> {
    let mut out = Vec::new();
    visit_entities(r, |entity| {
        out.push(entity);
        Ok(())
    })?;
    Ok(out)
}

fn visit_entities(r: &impl Read, mut each: impl FnMut(Entity) -> Result<()>) -> Result<()> {
    r.try_visit(&keys::all_entities(), |key, bytes| {
        let Some(Part::String(id)) = decode_key(key)?.pop() else {
            return Err(Error::new(ErrorCode::Corrupted, "An entity's key doesn't read"));
        };
        let record = entities::decode_record(&id, bytes)?;
        each(Entity { id, seq: record.seq, trashed: record.trashed, changed: record.changed })
    })
}

/// Hands `file` each key the index files each of these entities under as an entry, with the entity: the place
/// is the entity's
fn index_entries_into(
    r: &impl Read,
    entity_type: &str,
    index: &ValueIndex,
    of: &[&Entity],
    mut file: impl FnMut(Vec<u8>, &Entity) -> Result<()>,
) -> Result<()> {
    for e in of {
        let fields: Vec<Option<Value>> =
            index.fields.iter().map(|f| entities::get(r, &e.id, f)).collect::<Result<_>>()?;
        for key in index.files(&fields).entry_keys(entity_type, &index.name) {
            file(key, e)?;
        }
    }
    Ok(())
}

/// What an index that accounts for its entities holds besides its entries: each key it files each of these
/// entities under among those without a value in its last field, or among those holding a value it doesn't order
/// (`ValueIndex::files`)
fn index_others_into(
    r: &impl Read,
    entity_type: &str,
    index: &ValueIndex,
    of: &[&Entity],
    mut file: impl FnMut(Vec<u8>, &Entity) -> Result<()>,
) -> Result<()> {
    if !index.accounts() {
        return Ok(());
    }
    for e in of {
        let fields: Vec<Option<Value>> =
            index.fields.iter().map(|f| entities::get(r, &e.id, f)).collect::<Result<_>>()?;
        for key in index.files(&fields).other_keys(entity_type, &index.name) {
            file(key, e)?;
        }
    }
    Ok(())
}

/// Hands `file` each key an index's order has for each of these entities by its place: one for each string it
/// holds that has no key of its own (`entities::Filed::unplaced`)
fn order_places_into(
    r: &impl Read,
    entity_type: &str,
    index: &ValueIndex,
    of: &[&Entity],
    order: &super::order::Filing,
    mut file: impl FnMut(Vec<u8>) -> Result<()>,
) -> Result<()> {
    for e in of {
        let fields: Vec<Option<Value>> =
            index.fields.iter().map(|f| entities::get(r, &e.id, f)).collect::<Result<_>>()?;
        for key in order.of_places(entity_type, &index.name, &index.files(&fields).unplaced, e.seq) {
            file(key)?;
        }
    }
    Ok(())
}

/// Canonical representation of a value under `values::same` so equal values (including numbers of different
/// encodings, `-0.0` vs `+0.0`, and maps with reordered or repeated keys) encode to identical bytes
fn canonical_unique(value: &Value) -> Value {
    if let Some(n) = values::as_number(value) {
        return Value::F64(if n == 0.0 { 0.0 } else { n });
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_unique).collect()),
        Value::Map(fields) => {
            let mut entries: Vec<(Vec<u8>, Vec<u8>, Value, Value)> = fields
                .iter()
                .map(|(k, v)| {
                    let cv = canonical_unique(v);
                    (values::encode(k), values::encode(&cv), k.clone(), cv)
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            Value::Map(entries.into_iter().map(|(_, _, k, v)| (k, v)).collect())
        }
        other => other.clone(),
    }
}

fn canonical_unique_key(tuple: &[Value]) -> Vec<u8> {
    values::encode(&Value::Array(tuple.iter().map(canonical_unique).collect()))
}

/// Refuses a unique-index tuple that another entity already holds (`UNIQUE`), recording `own` (a canonical unique key)
/// under `id`
fn buildable_entry(
    entity_type: &str,
    index: &ValueIndex,
    own: &[u8],
    id: &str,
    holders: &mut BTreeMap<Vec<u8>, String>,
) -> Result<()> {
    match holders.get(own) {
        Some(other) if other != id => return Err(unbuildable(entity_type, &index.name, other, id)),
        Some(_) => {}
        None => {
            holders.insert(own.to_vec(), id.to_owned());
        }
    }
    Ok(())
}

/// Finds another entity of `entity_type` holding any of `wanted` canonical unique keys, for values that a value
/// index cannot file into `d/x` (long strings, maps, timestamps, binary)
fn unindexable_holder(
    t: &impl Read,
    entity_type: &str,
    index: &ValueIndex,
    id: &str,
    wanted: &[Vec<u8>],
) -> Result<Option<String>> {
    for trashed in [false, true] {
        for (_, other) in t.scan(&keys::type_range(trashed, entity_type))? {
            let other = values::decode(&other)?.as_str().unwrap_or_default().to_owned();
            if other == id {
                continue;
            }
            let fields: Vec<Option<Value>> =
                index.fields.iter().map(|f| entities::get(t, &other, f)).collect::<Result<_>>()?;
            if entities::unique_keys(&fields).iter().any(|k| wanted.contains(&canonical_unique_key(k))) {
                return Ok(Some(other));
            }
        }
    }
    Ok(None)
}

fn unbuildable(entity_type: &str, name: &str, one: &str, other: &str) -> Error {
    Error::new(
        ErrorCode::Unique,
        format!("The unique index {entity_type}.{name} can't be built: {one} and {other} hold the same values"),
    )
}

/// Removes every key of a value index: its entries, what it files besides them, and the blocks of each. The key
/// of the entities an index on one field holds without a value is the range's own first key, which no key under it
/// is
pub fn clear_value_index(t: &mut Txn<'_, '_>, entity_type: &str, name: &str) -> Result<()> {
    for range in keys::value_index_ranges(entity_type, name) {
        t.clear_prefix(&range)?;
        if t.contains(&range)? {
            t.del(&range)?;
        }
        t.clear_prefix(&keys::index_blocks(&range))?;
    }
    Ok(())
}

/// Whether an index of a type is built: its marker is there
pub fn is_built(r: &impl Read, entity_type: &str, index: &str) -> Result<bool> {
    r.contains(&keys::index_built(entity_type, index))
}

/// Says whether an index is built, and takes away the place a build of it had reached. The two go together: an
/// index said to be built, or unbuilt, was filed or left by something other than the build that reached that place,
/// over rows that may not be the ones it passed, so a build after it begins at the first
pub(super) fn mark(t: &mut Txn<'_, '_>, entity_type: &str, index: &str, built: bool) -> Result<()> {
    t.del(&keys::index_reached(entity_type, index))?;
    let marker = keys::index_built(entity_type, index);
    if built { t.put(&marker, &values::encode(&true.into())) } else { t.del(&marker) }
}

/// Emits what every derived key should hold: each type's live and trashed ranges and its count, every built index's
/// entries, and every relation's edges
fn derive_into(r: &impl Read, mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
    let all = entities_of(r)?;
    let mut live: BTreeMap<&str, i64> = BTreeMap::new();
    let mut by_type: BTreeMap<&str, Vec<&Entity>> = BTreeMap::new();
    for e in &all {
        let t = ids::type_of(&e.id);
        emit(keys::of_type(e.trashed, t, e.seq), values::encode(&e.id.as_str().into()))?;
        if super::changed::kept(r.schema(), t) {
            emit(keys::changed(t, e.changed, e.seq), values::encode(&e.id.as_str().into()))?;
        }
        if !e.trashed {
            *live.entry(t).or_default() += 1;
        }
        by_type.entry(t).or_default().push(e);
    }
    for (t, n) in live {
        emit(keys::count(t), values::encode(&n.into()))?;
    }
    for (t, of) in &by_type {
        for index in value_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                // Its order's keys with its entries, where a read trusts the order: under a marker of another
                // stamp, or one not whole, what the order holds is no one's to say here
                let order = super::order::trusted(r, t, &index.name)?;
                let mut filing = places::Sorting::new()?.ordered(order.clone());
                index_entries_into(r, t, &index, of, |key, e| filing.entry(key, e.seq))?;
                index_others_into(r, t, &index, of, |key, e| filing.entry(key, e.seq))?;
                filing.finish(&mut emit)?;
                if let Some(order) = &order {
                    order_places_into(r, t, &index, of, order, |key| emit(key, Vec::new()))?;
                }
            }
        }
        let places: Vec<(u64, String)> = of.iter().map(|e| (e.seq, e.id.clone())).collect();
        for index in text::text_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                text::entries_into(r, t, &index, &places, &mut emit)?;
            }
        }
        for index in trigram::trigram_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                trigram::entries_into(r, t, &index, &places, &mut emit)?;
            }
        }
        let ids: Vec<String> = of.iter().map(|e| e.id.clone()).collect();
        for index in vector::vector_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                vector::entries_into(r, t, &index, &ids, &mut emit)?;
            }
        }
    }
    let mut fields = BTreeSet::new();
    for (key, _) in r.scan(&keys::all_values())? {
        if let [_, Part::String(id), Part::String(field)] = decode_key(&key)?.as_slice() {
            fields.insert(keys::field_name(ids::type_of(id), field));
        }
    }
    for key in fields {
        emit(key, Vec::new())?;
    }
    let mut kinds: BTreeMap<String, i64> = BTreeMap::new();
    for (key, bytes) in r.scan(&keys::all_relations())? {
        let relation = match decode_key(&key)?.pop() {
            Some(Part::String(id)) => id,
            _ => return Err(Error::new(ErrorCode::Corrupted, "A relation's key doesn't read")),
        };
        let s: relations::Stored = rmp_serde::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("Relation {relation}'s row doesn't read: {e}")))?;
        emit(keys::edge(true, &s.source, &s.kind, s.seq), relations::edge_value(&relation, &s.target, s.weight))?;
        emit(keys::edge(false, &s.target, &s.kind, s.seq), relations::edge_value(&relation, &s.source, s.weight))?;
        emit(keys::between(&s.source, &s.kind, &s.target, s.seq), values::encode(&relation.as_str().into()))?;
        *kinds.entry(s.kind).or_default() += 1;
    }
    for (kind, n) in kinds {
        emit(keys::relation_count(&kind), values::encode(&n.into()))?;
    }
    Ok(())
}

/// The derived prefixes `derive` answers for, and so `check` compares: all but the index markers and the place a
/// build has reached, which record what was built rather than following from primary data
const COMPARED: [Prefix; 27] = [
    Prefix::Live,
    Prefix::Changed,
    Prefix::Trashed,
    Prefix::Count,
    Prefix::RelationCount,
    Prefix::FieldName,
    Prefix::Index,
    Prefix::IndexPlaces,
    Prefix::IndexWithout,
    Prefix::IndexWithoutPlaces,
    Prefix::IndexUnordered,
    Prefix::IndexUnorderedPlaces,
    Prefix::IndexLong,
    Prefix::IndexLongPlaces,
    Prefix::IndexOrder,
    Prefix::Out,
    Prefix::In,
    Prefix::Between,
    Prefix::TextTerm,
    Prefix::TextBlock,
    Prefix::TextDocument,
    Prefix::TextLengths,
    Prefix::TextStats,
    Prefix::TextWord,
    Prefix::Trigram,
    Prefix::VectorChunk,
    Prefix::VectorPending,
];

/// The prefixes whose keys belong to one index of a type, its second and third parts, compared only once it is built
const PER_INDEX: [Prefix; 18] = [
    Prefix::Index,
    Prefix::IndexPlaces,
    Prefix::IndexWithout,
    Prefix::IndexWithoutPlaces,
    Prefix::IndexUnordered,
    Prefix::IndexUnorderedPlaces,
    Prefix::IndexLong,
    Prefix::IndexLongPlaces,
    Prefix::IndexOrder,
    Prefix::TextTerm,
    Prefix::TextBlock,
    Prefix::TextDocument,
    Prefix::TextLengths,
    Prefix::TextStats,
    Prefix::TextWord,
    Prefix::Trigram,
    Prefix::VectorChunk,
    Prefix::VectorPending,
];

/// Every index of a type that is built or waits to be: its value indexes and its text indexes
pub fn index_names(schema: &Schema, entity_type: &str) -> Vec<String> {
    let mut names: Vec<String> = value_indexes(schema, entity_type).into_iter().map(|i| i.name).collect();
    names.extend(text::text_indexes(schema, entity_type).into_iter().map(|i| i.name));
    names.extend(vector::vector_indexes(schema, entity_type).into_iter().map(|i| i.name));
    names.extend(trigram::trigram_indexes(schema, entity_type).into_iter().map(|i| i.name));
    names
}

/// Whether a stored derived key participates in comparison; unbuilt index contents are intentionally ignored.
fn compared(r: &impl Read, prefix: Prefix, key: &[u8]) -> Result<bool> {
    if !PER_INDEX.contains(&prefix) {
        return Ok(true);
    }
    let parts = decode_key(key)?;
    match (parts.get(1), parts.get(2)) {
        // An order's keys are compared under the build a read trusts: one of another stamp is that stamp's to lay
        // out, one not whole is still being filed, and a key under any other build is waiting to be deleted
        (Some(Part::String(t)), Some(Part::String(i))) if prefix == Prefix::IndexOrder => {
            let build = super::order::readable(r, t, i)?;
            Ok(is_built(r, t, i)?
                && build.is_some()
                && parts.get(3) == build.map(|build| Part::Number(build as f64)).as_ref())
        }
        (Some(Part::String(t)), Some(Part::String(i))) => is_built(r, t, i),
        _ => Ok(false),
    }
}

/// One derived key that disagrees with primary data
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Difference {
    /// The key's parts, as text
    pub key: String,
    /// `missing` (derived, not stored), `extra` (stored, not derived) or `differs`
    pub kind: String,
}

/// Something stored that breaks a rule of the schema: a row that predates a tightened rule, a reference to nothing
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    /// The entity or relation
    pub subject: String,
    pub message: String,
}

/// What `check()` found
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CheckReport {
    pub differences: Vec<Difference>,
    pub violations: Vec<Violation>,
    /// What the check did not compare, and why: the order a value index keeps of its strings, where this elmdb
    /// did not file it or it is not whole. Nothing is wrong with what a note names; the report is clean with them
    #[serde(default)]
    pub notes: Vec<String>,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty() && self.violations.is_empty()
    }
}

pub(super) fn key_text(key: &[u8]) -> String {
    match decode_key(key) {
        Ok(parts) => parts
            .iter()
            .map(|p| match p {
                Part::String(s) => s.clone(),
                Part::Number(n) => format!("{n}"),
                Part::Bool(b) => b.to_string(),
                Part::Null => "null".to_owned(),
                Part::Bytes(b) => format!("{b:02x?}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        Err(_) => format!("{key:02x?}"),
    }
}

/// Compares every derived key with what primary data derives, and every row with the schema's rules
pub fn check(r: &impl Read) -> Result<CheckReport> {
    let mut scratch = sorted::Scratch::new()?;
    derive_into(r, |key, value| scratch.push(key, value))?;
    let mut expected = scratch.finish()?;
    let mut next = expected.next()?;
    let mut changed = Vec::new();
    let mut extra = Vec::new();
    let mut prefixes = COMPARED;
    prefixes.sort_by_key(|prefix| prefix.key().to_vec());
    for prefix in prefixes {
        let mut failed = None;
        r.visit(&prefix.key().to_vec(), |key, value| {
            let step = (|| -> Result<()> {
                if !compared(r, prefix, key)? {
                    return Ok(());
                }
                while next.as_ref().is_some_and(|(wanted, _)| wanted.as_slice() < key) {
                    let (missing, _) = next.take().expect("next exists");
                    changed.push(Difference { key: key_text(&missing), kind: "missing".into() });
                    next = expected.next()?;
                }
                if next.as_ref().is_some_and(|(wanted, _)| wanted.as_slice() == key) {
                    let (_, wanted) = next.take().expect("next exists");
                    if wanted != value {
                        changed.push(Difference { key: key_text(key), kind: "differs".into() });
                    }
                    next = expected.next()?;
                } else if prefix != Prefix::FieldName {
                    // A registered field stays when its last value goes, so only one missing is a difference
                    extra.push(Difference { key: key_text(key), kind: "extra".into() });
                }
                Ok(())
            })();
            if let Err(error) = step {
                failed = Some(error);
                return false;
            }
            true
        })?;
        if let Some(error) = failed {
            return Err(error);
        }
    }
    while let Some((key, _)) = next {
        changed.push(Difference { key: key_text(&key), kind: "missing".into() });
        next = expected.next()?;
    }
    let differences = changed.into_iter().chain(extra).collect();
    let mut violations = audit::violations(r)?;
    for message in vector::check_items(r, r.schema())? {
        violations.push(Violation { subject: "vector graph".into(), message });
    }
    // A rank index's bits depend on the order of the commits that made it, so it is held to what it promises rather
    // than compared with keys derived again
    violations.extend(super::ranks::violations(r)?);
    Ok(CheckReport { differences, violations, notes: uncompared(r)? })
}

/// Each order of a built value index that `check` compared no key of, with why: one filed under another stamp,
/// whose keys are that elmdb's to lay out, one being built, and one not begun
fn uncompared(r: &impl Read) -> Result<Vec<String>> {
    let stamp = super::order::stamp(&r.schema().collation)?;
    let mut notes = Vec::new();
    for entity_type in r.schema().types.keys() {
        for index in value_indexes(r.schema(), entity_type).into_iter().filter(ValueIndex::accounts) {
            if !is_built(r, entity_type, &index.name)? {
                continue;
            }
            let why = match super::order::marker(r, entity_type, &index.name)? {
                Some(marker) if marker.stamp != stamp => "was filed under another stamp",
                Some(marker) if !marker.whole => "is being built",
                Some(_) => continue,
                None => "is not built yet",
            };
            notes.push(format!("The order of {entity_type}.{}'s strings {why}, and was not compared", index.name));
        }
    }
    Ok(notes)
}

/// Deletes every compared derived key, and with `index` only that index's entries, then writes what primary data
/// derives, marking the indexes it built. One index is refused when the schema doesn't declare it (`UNKNOWN_INDEX`) or
/// a build of it couldn't finish (`buildable`); a rebuild of everything, which a rename and a replace run, leaves such
/// a value index unbuilt instead, holding the entries that fit, as a build that failed does
pub fn rebuild(t: &mut Txn<'_, '_>, index: Option<(&str, &str)>) -> Result<()> {
    match index {
        Some((entity_type, name)) => {
            if !t.schema.types.contains_key(entity_type) {
                return Err(Error::new(
                    ErrorCode::UnknownType,
                    format!("rebuild: \"{entity_type}\" isn't a type the schema declares"),
                ));
            }
            if !index_names(t.schema, entity_type).iter().any(|i| i == name) {
                return Err(Error::new(
                    ErrorCode::UnknownIndex,
                    format!("rebuild: {entity_type} has no index \"{name}\""),
                ));
            }
            // Stage from primary records before mutating derived data. The write transaction makes the entire
            // replacement atomic, including a late uniqueness, scratch or graph failure.
            if let Some(found) = value_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) {
                let mut filing = places::Sorting::new()?;
                let mut holders = if found.unique { Some(sorted::Scratch::new()?) } else { None };
                visit_entities(t, |e| {
                    if ids::type_of(&e.id) != entity_type {
                        return Ok(());
                    }
                    let fields: Vec<Option<Value>> =
                        found.fields.iter().map(|f| entities::get(t, &e.id, f)).collect::<Result<_>>()?;
                    if let Some(holders) = holders.as_mut() {
                        // Enforce uniqueness over every `entities::unique_keys` tuple, including values the index
                        // cannot file (strings past `MAX_INDEXED_STRING`, maps, timestamps, binary). Include `e.id`
                        // in the sort key because `Scratch` deduplicates identical keys.
                        for tuple in entities::unique_keys(&fields) {
                            let holder = [Part::Bytes(canonical_unique_key(&tuple)), Part::String(e.id.clone())];
                            holders.push(crate::format::key::encode(&holder), Vec::new())?;
                        }
                    }
                    let filed = found.files(&fields);
                    for key in filed.other_keys(entity_type, name) {
                        entities::key_fits(&key, &e.id, entity_type, name)?;
                        filing.entry(key, e.seq)?;
                    }
                    for key in filed.entry_keys(entity_type, name) {
                        entities::key_fits(&key, &e.id, entity_type, name)?;
                        filing.entry(key, e.seq)?;
                    }
                    Ok(())
                })?;
                if let Some(holders) = holders {
                    let mut holders = holders.finish()?;
                    let mut prev: Option<(Vec<u8>, String)> = None;
                    while let Some((order, _)) = holders.next()? {
                        let mut parts = decode_key(&order)?.into_iter();
                        let (Some(Part::Bytes(key)), Some(Part::String(id))) = (parts.next(), parts.next()) else {
                            return Err(Error::new(ErrorCode::Corrupted, "A sorted index holder doesn't read"));
                        };
                        if let Some((held, other)) = &prev
                            && *held == key
                            && *other != id
                        {
                            return Err(unbuildable(entity_type, name, other, &id));
                        }
                        prev = Some((key, id));
                    }
                }
                clear_value_index(t, entity_type, name)?;
                text::clear(t, entity_type, name)?;
                // Its order with it, whole under a build of this unit's: the keys of its entries' strings from the
                // entries as they are laid out, and those of the entities filed by their places from their rows
                super::order::clear(t, entity_type, name)?;
                let order = match found.accounts() {
                    true => Some(super::order::begin_whole(t, entity_type, name)?),
                    false => None,
                };
                filing.ordered(order.clone()).finish(|key, value| t.put(&key, &value))?;
                if let Some(order) = &order {
                    let mut places = Vec::new();
                    visit_entities(t, |e| match ids::type_of(&e.id) == entity_type {
                        true => order_places_into(t, entity_type, &found, &[&e], order, |key| {
                            places.push(key);
                            Ok(())
                        }),
                        false => Ok(()),
                    })?;
                    for key in places {
                        t.put(&key, &[])?;
                    }
                }
            } else if let Some(found) = text::text_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) {
                let mut scratch = sorted::Scratch::new()?;
                let mut text = text::Entries::new(entity_type, &found)?;
                visit_entities(t, |e| {
                    if ids::type_of(&e.id) == entity_type {
                        text.entity(t, e.seq, &e.id, |key, value| scratch.push(key, value))?;
                    }
                    Ok(())
                })?;
                text.finish(|key, value| scratch.push(key, value))?;
                let mut entries = scratch.finish()?;
                t.clear_prefix(&keys::index_range(entity_type, name, &[]))?;
                text::clear(t, entity_type, name)?;
                while let Some((key, value)) = entries.next()? {
                    t.put(&key, &value)?;
                }
            } else if let Some(found) =
                trigram::trigram_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name)
            {
                let mut scratch = sorted::Scratch::new()?;
                let mut grams = trigram::Entries::new(entity_type, &found)?;
                visit_entities(t, |e| {
                    if ids::type_of(&e.id) == entity_type {
                        grams.entity(t, e.seq, &e.id)?;
                    }
                    Ok(())
                })?;
                grams.finish(|key, value| scratch.push(key, value))?;
                let mut entries = scratch.finish()?;
                trigram::clear(t, entity_type, name)?;
                while let Some((key, value)) = entries.next()? {
                    t.put(&key, &value)?;
                }
            } else if let Some(found) =
                vector::vector_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name)
            {
                let mut scratch = sorted::Scratch::new()?;
                visit_entities(t, |e| {
                    if ids::type_of(&e.id) == entity_type {
                        scratch.push(e.id.into_bytes(), Vec::new())?;
                    }
                    Ok(())
                })?;
                let mut entities = scratch.finish()?;
                t.clear_prefix(&keys::index_range(entity_type, name, &[]))?;
                text::clear(t, entity_type, name)?;
                vector::clear(t, entity_type, name)?;
                while let Some((id, _)) = entities.next()? {
                    let id = std::str::from_utf8(&id)
                        .map_err(|_| Error::new(ErrorCode::Corrupted, "A sorted vector entity id doesn't read"))?;
                    vector::refile_at(t, entity_type, &found, id)?;
                }
                vector::flush(t)?;
            }
            mark(t, entity_type, name, true)?;
        }
        None => {
            for prefix in COMPARED {
                t.clear_prefix(&prefix.key().to_vec())?;
            }
            // Every order's marker with its keys: an index built below has its order whole under a build of this
            // unit's, and one left unbuilt has neither
            t.clear_prefix(&Prefix::IndexOrderBuilt.key().to_vec())?;
            // And the place every build had reached, over entries that are no longer there
            t.clear_prefix(&Prefix::IndexReached.key().to_vec())?;
            // What an earlier layout left under a tag nothing reads now
            for retired in Prefix::retired() {
                t.clear_prefix(&retired)?;
            }
            // Every declared index of every type with entities is built by a full rebuild, but a value index a build
            // couldn't finish, which keeps what it files that fits, unmarked, as writes keep an unbuilt index's
            let all = entities_of(t)?;
            let mut by_type: BTreeMap<String, Vec<&Entity>> = BTreeMap::new();
            for e in &all {
                by_type.entry(ids::type_of(&e.id).to_owned()).or_default().push(e);
            }
            for (ty, of) in &by_type {
                let value: Vec<ValueIndex> = value_indexes(t.schema, ty);
                for index in index_names(t.schema, ty) {
                    let Some(found) = value.iter().find(|i| i.name == index) else {
                        mark(t, ty, &index, true)?;
                        continue;
                    };
                    let mut holders: BTreeMap<Vec<u8>, String> = BTreeMap::new();
                    // Check every unique key (indexable and unindexable alike) and every key the index files
                    let buildable = (|| -> Result<()> {
                        for e in of {
                            let fields: Vec<Option<Value>> =
                                found.fields.iter().map(|f| entities::get(t, &e.id, f)).collect::<Result<_>>()?;
                            if found.unique {
                                for tuple in entities::unique_keys(&fields) {
                                    buildable_entry(ty, found, &canonical_unique_key(&tuple), &e.id, &mut holders)?;
                                }
                            }
                            let filed = found.files(&fields);
                            for key in filed.entry_keys(ty, &found.name) {
                                entities::key_fits(&key, &e.id, ty, &found.name)?;
                            }
                            for key in filed.other_keys(ty, &found.name) {
                                entities::key_fits(&key, &e.id, ty, &found.name)?;
                            }
                        }
                        Ok(())
                    })();
                    match buildable {
                        Ok(()) => mark(t, ty, &index, true)?,
                        Err(e) if matches!(e.code, ErrorCode::Unique | ErrorCode::ValueInvalid) => {
                            mark(t, ty, &index, false)?;
                            let mut fallback = places::Sorting::new()?;
                            let mut fits = |key: Vec<u8>, e: &Entity| {
                                if entities::key_fits(&key, &e.id, ty, &found.name).is_ok() {
                                    fallback.entry(key, e.seq)?;
                                }
                                Ok(())
                            };
                            index_entries_into(t, ty, found, of, &mut fits)?;
                            index_others_into(t, ty, found, of, &mut fits)?;
                            fallback.finish(|key, value| t.put(&key, &value))?;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            // The order of every built index a schema declares, a type with no entity's among them, which
            // `derive_into` then files the keys of
            let declared: Vec<(String, String)> = t
                .schema
                .types
                .keys()
                .flat_map(|ty| value_indexes(t.schema, ty).into_iter().map(move |index| (ty.clone(), index)))
                .filter(|(_, index)| index.accounts())
                .map(|(ty, index)| (ty, index.name))
                .collect();
            for (ty, index) in declared {
                if is_built(t, &ty, &index)? {
                    super::order::begin_whole(t, &ty, &index)?;
                }
            }
            let mut scratch = sorted::Scratch::new()?;
            derive_into(t, |key, value| scratch.push(key, value))?;
            let mut entries = scratch.finish()?;
            while let Some((key, value)) = entries.next()? {
                t.put(&key, &value)?;
            }
            vector::rebuild_graphs(t)?;
            // Once the edges and the types' ranges are written again, from which each rank index's nodes are made
            let names: Vec<String> = t.schema.ranks.keys().cloned().collect();
            for name in names {
                super::ranks::rebuild(t, &name)?;
            }
        }
    }
    Ok(())
}

/// Seek both type ranges after the saved place, reading at most one chunk plus lookahead from each. Only these
/// bounded candidates are sorted; earlier chunks are never walked again. At fixed chunk size visits grow linearly
/// with the population, and temporary row storage is at most 2 * (limit + 1).
fn build_rows(t: &Txn<'_, '_>, entity_type: &str, after: u64, limit: usize) -> Result<(Vec<(u64, String)>, usize)> {
    let mut candidates = Vec::new();
    let mut visited = 0;
    let (txn, db) = t.raw();
    for trashed in [false, true] {
        let low = keys::of_type(trashed, entity_type, after);
        let (_, high) = crate::format::key::prefix_range(&keys::type_range(trashed, entity_type));
        let range = (std::ops::Bound::Excluded(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
        for entry in db.range(txn, &range)?.take(limit.saturating_add(1)) {
            let (key, id) = entry?;
            visited += 1;
            let seq = match decode_key(key)?.pop() {
                Some(Part::Number(n)) => n as u64,
                _ => return Err(Error::new(ErrorCode::Corrupted, "An entity's type-range place doesn't read")),
            };
            candidates.push((seq, values::decode(id)?.as_str().unwrap_or_default().to_owned()));
        }
    }
    candidates.sort_by_key(|(seq, _)| *seq);
    candidates.truncate(limit.saturating_add(1));
    Ok((candidates, visited))
}

/// One chunk of an index's build: at most `limit` of the type's entities, from the place the build has reached,
/// answering whether the whole type is done (and the index marked built). Refuses an entry a unique index holds for
/// another entity (`UNIQUE`), which leaves the index unbuilt.
///
/// The place reached is kept in the store (`keys::index_reached`), written by the chunk that reached it and read by
/// the next, and not by whoever runs the chunks. A place is one among the rows a build has passed, and means nothing
/// over others: a replacement of every row, a rebuild or a rename leaves the index unbuilt where its rows can't be
/// filed, and a build that went on from a place it had kept would pass those rows and mark it built. Each of them
/// takes the place away with the entries it replaces (`mark`), so the build after it begins at the first row,
/// whichever process made the change. A build a close stopped goes on from its place at the next open
pub fn build_chunk(t: &mut Txn<'_, '_>, entity_type: &str, name: &str, limit: usize) -> Result<bool> {
    if is_built(t, entity_type, name)? {
        return Ok(true);
    }
    let reached = keys::index_reached(entity_type, name);
    let after = match t.get(&reached)? {
        Some(place) => values::decode(&place)?.as_u64().ok_or_else(|| {
            Error::new(
                ErrorCode::Corrupted,
                format!(
                    "The place the build of {entity_type}.{name} reached doesn't read: rebuild() takes it away and \
                     builds the index"
                ),
            )
        })?,
        None => 0,
    };
    match build_chunk_after(t, entity_type, name, after, limit)? {
        Some(last) => {
            t.put(&reached, &values::encode(&last.into()))?;
            Ok(false)
        }
        None => Ok(true),
    }
}

/// `build_chunk` from the place `after`: the last place built, or `None` when the whole type is done
fn build_chunk_after(
    t: &mut Txn<'_, '_>,
    entity_type: &str,
    name: &str,
    after: u64,
    limit: usize,
) -> Result<Option<u64>> {
    let (chunk, _) = build_rows(t, entity_type, after, limit)?;
    let done = chunk.len() <= limit;
    let mut last = after;
    let Some(index) = value_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) else {
        if let Some(index) = vector::vector_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) {
            for (seq, id) in chunk.iter().take(limit) {
                vector::refile_at(t, entity_type, &index, id)?;
                last = *seq;
            }
            vector::flush(t)?;
            if done {
                mark(t, entity_type, name, true)?;
                return Ok(None);
            }
            return Ok(Some(last));
        }
        if let Some(index) = trigram::trigram_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) {
            for (seq, id) in chunk.iter().take(limit) {
                trigram::refile_at(t, entity_type, &index, id, *seq)?;
                last = *seq;
            }
            if done {
                mark(t, entity_type, name, true)?;
                return Ok(None);
            }
            return Ok(Some(last));
        }
        let Some(index) = text::text_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) else {
            return Ok(None);
        };
        for (seq, id) in chunk.iter().take(limit) {
            text::refile_at(t, entity_type, &index, id, *seq)?;
            last = *seq;
        }
        if done {
            mark(t, entity_type, name, true)?;
            return Ok(None);
        }
        return Ok(Some(last));
    };
    for (seq, id) in chunk.iter().take(limit) {
        let fields: Vec<Option<Value>> = index.fields.iter().map(|f| entities::get(t, id, f)).collect::<Result<_>>()?;
        let filed = index.files(&fields);
        if index.unique {
            let unindexable: Vec<Vec<u8>> = entities::unique_keys(&fields)
                .into_iter()
                .filter(|tuple| tuple.iter().any(|v| values::index_part(v).is_none()))
                .map(|tuple| canonical_unique_key(&tuple))
                .collect();
            if !unindexable.is_empty()
                && let Some(other) = unindexable_holder(t, entity_type, &index, id, &unindexable)?
            {
                return Err(unbuildable(entity_type, name, &other, id));
            }
        }
        for key in filed.entry_keys(entity_type, name) {
            if index.unique {
                // A write checks a unique value against the rows while the index is unbuilt, so another entity
                // filed under the values here is one the build can't go on past
                for place in &places::under(t, &key)? {
                    if place != *seq
                        && let Some((other, _)) = entities::at_place(t, entity_type, place)?
                    {
                        return Err(unbuildable(entity_type, name, &other, id));
                    }
                }
            }
            entities::key_fits(&key, id, entity_type, name)?;
            places::file(t, key, *seq)?;
        }
        for key in filed.other_keys(entity_type, name) {
            entities::key_fits(&key, id, entity_type, name)?;
            places::file(t, key, *seq)?;
        }
        last = *seq;
    }
    if done {
        mark(t, entity_type, name, true)?;
        return Ok(None);
    }
    Ok(Some(last))
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    fn recovery_emitters_stop_before_decoding_the_next_entity() {
        use crate::engine::tests::{open, write};
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut declared = db.schema().unwrap();
        let mut json = serde_json::to_value(&declared).unwrap();
        json["types"]["Task"]["indexes"]["meaning"] =
            serde_json::json!({"kind":"vector","from":["title"],"dims":2,"model":"m"});
        declared = Schema::from_json(&json).unwrap();
        db.set_schema(declared).unwrap();
        write(
            &db,
            vec![
                serde_json::json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":1,"title":"first"}}),
                serde_json::json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":2,"title":"second"}}),
            ],
        )
        .unwrap();
        db.store()
            .unwrap()
            .write(
                |txn, main| {
                    main.put(txn, &keys::value("Task-1", "a"), &[])?;
                    main.put(txn, &keys::value("Task-1", "title"), &[])?;
                    Ok(())
                },
                false,
            )
            .unwrap();
        db.snapshot(|r| {
            let all = entities_of(r)?;
            let of: Vec<_> = all.iter().collect();
            let index = value_indexes(r.schema(), "Task").into_iter().find(|i| i.name == "a").unwrap();
            let stopped = || Error::new(ErrorCode::Io, "stop after the first emitted entry");
            let error = index_entries_into(r, "Task", &index, &of, |_, _| Err(stopped())).unwrap_err();
            assert_eq!(error.message, stopped().message);
            let vector = vector::vector_indexes(r.schema(), "Task").remove(0);
            let error =
                vector::entries_into(r, "Task", &vector, &["Task-0".into(), "Task-1".into()], |_, _| Err(stopped()))
                    .unwrap_err();
            assert_eq!(error.message, stopped().message);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn chunk_cursors_visit_linear_rows_across_mixed_live_and_trash_ranges() {
        use super::super::unit::UnitState;
        use crate::store::{Store, StoreOptions};
        let schema = Schema::from_json(&serde_json::json!({"$schema":crate::schema::SCHEMA_LANGUAGE})).unwrap();
        for size in [1024u64, 4096, 16384] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path(), StoreOptions { syncs: false, ..Default::default() }).unwrap();
            store
                .write(
                    |txn, db| {
                        for seq in 1..=size {
                            db.put(
                                txn,
                                &keys::of_type(seq % 3 == 0, "Node", seq),
                                &values::encode(&format!("Node-{seq}").into()),
                            )?;
                        }
                        let mut state = UnitState::new(false);
                        let t = Txn { txn, db, state: &mut state, schema: &schema, now: 0.0 };
                        let mut after = 0;
                        let mut visited = 0;
                        let mut count = 0;
                        loop {
                            let (rows, visits) = build_rows(&t, "Node", after, 64)?;
                            visited += visits;
                            assert!(rows.len() <= 65);
                            let done = rows.len() <= 64;
                            for (seq, id) in rows.into_iter().take(64) {
                                count += 1;
                                assert_eq!(seq, count);
                                assert_eq!(id, format!("Node-{count}"));
                                after = seq;
                            }
                            if done {
                                break;
                            }
                        }
                        assert_eq!(count, size);
                        assert!(visited <= 3 * size as usize, "rows={size}, visited={visited}");
                        assert!(visited >= size as usize, "every row is read: rows={size}, visited={visited}");
                        println!("rows={size}, cursor_entries={visited}, chunk=64");
                        Ok(())
                    },
                    false,
                )
                .unwrap();
        }
    }

    #[test]
    fn unique_index_build_admits_distinct_and_refuses_duplicate_unindexable_values() {
        use crate::engine::batch::{Batch, Op};
        use crate::{Database, OpenOptions};
        use serde_json::json;
        use std::collections::BTreeMap;

        // Canonicalization normalizes -0.0 and distinct numbers stay distinct
        assert_eq!(canonical_unique_key(&[Value::F64(-0.0)]), canonical_unique_key(&[Value::from(0)]));
        assert_ne!(canonical_unique_key(&[Value::from(1.0)]), canonical_unique_key(&[Value::from(2.0)]));

        let plain = Schema::from_json(&json!({
            "$schema": crate::schema::SCHEMA_LANGUAGE,
            "types": { "Item": { "fields": { "k": { "type": "any", "optional": true } } } }
        }))
        .unwrap();
        let unique = Schema::from_json(&json!({
            "$schema": crate::schema::SCHEMA_LANGUAGE,
            "types": {
                "Item": {
                    "fields": { "k": { "type": "any", "optional": true } },
                    "indexes": { "k": { "kind": "value", "fields": ["k"], "unique": true } }
                }
            }
        }))
        .unwrap();
        let create = |id: &str, v: Value| Op::Create {
            entity_type: "Item".into(),
            id: Some(id.into()),
            short_code: None,
            created_at: None,
            updated_at: None,
            fields: BTreeMap::from([("k".into(), v)]),
        };

        // Distinct unindexable values (long strings, maps, timestamps): build and rebuild succeed
        let dir = tempfile::tempdir().unwrap();
        let db =
            Database::open(dir.path(), OpenOptions { schema: Some(plain.clone()), syncs: false, ..Default::default() })
                .unwrap();
        db.write(
            &Batch {
                ops: vec![
                    create("Item-1", Value::from("a".repeat(300))),
                    create("Item-2", Value::from("b".repeat(300))),
                    create("Item-3", Value::Map(vec![("x".into(), 1.into())])),
                    create("Item-4", Value::Map(vec![("x".into(), 2.into())])),
                ],
                now: Some(1_000.0),
            },
            false,
        )
        .unwrap();
        db.set_schema(unique.clone()).unwrap();
        db.wait_for_index_builds().unwrap();
        db.rebuild(Some(("Item", "k"))).unwrap();
        db.rebuild(None).unwrap();
        assert!(db.check().unwrap().is_clean());

        // Duplicate unindexable values: build fails, single-index rebuild returns UNIQUE
        let dir2 = tempfile::tempdir().unwrap();
        let db2 = Database::open(dir2.path(), OpenOptions { schema: Some(plain), syncs: false, ..Default::default() })
            .unwrap();
        let dup = Value::from("z".repeat(300));
        db2.write(
            &Batch { ops: vec![create("Item-1", dup.clone()), create("Item-2", dup)], now: Some(1_000.0) },
            false,
        )
        .unwrap();
        db2.set_schema(unique).unwrap();
        assert_eq!(db2.wait_for_index_builds().unwrap_err().code, ErrorCode::IndexBuildFailed);
        assert_eq!(db2.rebuild(Some(("Item", "k"))).unwrap_err().code, ErrorCode::Unique);
    }

    #[test]
    fn cycle_audit_handles_a_long_chain_and_parallel_forward_edges() {
        let mut graph = PrimaryGraph::new();
        for i in 0..20_000 {
            graph.insert(i.to_string(), vec![((i + 1).to_string(), format!("edge-{i}"))]);
        }
        graph.get_mut("0").unwrap().push(("1".into(), "parallel".into()));
        let mut violations = Vec::new();
        audit_cycles("chain", &graph, &mut violations);
        assert!(violations.is_empty());
        graph.insert("20000".into(), vec![("0".into(), "back-edge".into())]);
        audit_cycles("chain", &graph, &mut violations);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].subject, "back-edge");
    }
}
