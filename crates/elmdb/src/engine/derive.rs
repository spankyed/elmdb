//! Derived data from primary data alone (`docs/architecture/format.md`, Keys): what every derived key should hold, which `check()`
//! compares with what is stored and `rebuild()` writes in its place. An index is read only once built (its `d/xb`
//! marker); writes keep every index whether built or not, so building one fills in what predates it.

use std::collections::BTreeMap;

use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};
use crate::format::Prefix;
use crate::format::key::{Part, decode as decode_key};
use crate::schema::{Cardinality, Schema};

use super::entities::{self, ValueIndex, entries_of, value_indexes};
use super::keys;
use super::unit::{Read, Txn};
use super::{ids, relations, text, values, vector};

/// One entity as primary data holds it
struct Entity {
    id: String,
    seq: u64,
    trashed: bool,
}

/// Every entity the store holds, from `p/i`
fn entities_of(r: &impl Read) -> Result<Vec<Entity>> {
    let mut out = Vec::new();
    for (key, bytes) in r.scan(&keys::all_entities())? {
        let id = match decode_key(&key)?.pop() {
            Some(Part::String(id)) => id,
            _ => return Err(Error::new(ErrorCode::Corrupted, "An entity's key doesn't read")),
        };
        let record = values::decode(&bytes)?;
        let items =
            record.as_array().ok_or_else(|| Error::new(ErrorCode::Corrupted, format!("{id}'s record doesn't read")))?;
        let seq = items.first().and_then(Value::as_u64).unwrap_or(0);
        let trashed = items.get(1).and_then(Value::as_bool).unwrap_or(false);
        out.push(Entity { id, seq, trashed });
    }
    Ok(out)
}

/// The entries one index of a type should hold, from its entities' fields
fn index_entries(
    r: &impl Read,
    entity_type: &str,
    index: &ValueIndex,
    of: &[&Entity],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    for e in of {
        let fields: Vec<Option<Value>> =
            index.fields.iter().map(|f| entities::get(r, &e.id, f)).collect::<Result<_>>()?;
        for parts in entries_of(&fields) {
            out.push((
                keys::index_entry(entity_type, &index.name, &parts, e.seq),
                values::encode(&e.id.as_str().into()),
            ));
        }
    }
    Ok(out)
}

/// Refuses what would keep a build of a value index from finishing: an entry whose key LMDB can't store
/// (`VALUE_INVALID`), or, in a unique index, two entities filed under the same values (`UNIQUE`)
fn buildable(entity_type: &str, index: &ValueIndex, entries: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    let mut holders: BTreeMap<Vec<u8>, String> = BTreeMap::new();
    for (key, id) in entries {
        let id = values::decode(id)?.as_str().unwrap_or_default().to_owned();
        entities::key_fits(key, &id, entity_type, &index.name)?;
        if !index.unique {
            continue;
        }
        let mut parts = decode_key(key)?;
        parts.drain(..3);
        parts.pop();
        match holders.get(&crate::format::key::encode(&parts)) {
            Some(other) if *other != id => return Err(unbuildable(entity_type, &index.name, other, &id)),
            Some(_) => {}
            None => {
                holders.insert(crate::format::key::encode(&parts), id);
            }
        }
    }
    Ok(())
}

fn unbuildable(entity_type: &str, name: &str, one: &str, other: &str) -> Error {
    Error::new(
        ErrorCode::Unique,
        format!("The unique index {entity_type}.{name} can't be built: {one} and {other} hold the same values"),
    )
}

/// Whether an index of a type is built: its marker is there
pub fn is_built(r: &impl Read, entity_type: &str, index: &str) -> Result<bool> {
    r.contains(&keys::index_built(entity_type, index))
}

/// What every derived key should hold: each type's live and trashed ranges and its count, every built index's entries,
/// and every relation's edges
pub fn derive(r: &impl Read) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let all = entities_of(r)?;
    let mut live: BTreeMap<&str, i64> = BTreeMap::new();
    let mut by_type: BTreeMap<&str, Vec<&Entity>> = BTreeMap::new();
    for e in &all {
        let t = ids::type_of(&e.id);
        out.insert(keys::of_type(e.trashed, t, e.seq), values::encode(&e.id.as_str().into()));
        if !e.trashed {
            *live.entry(t).or_default() += 1;
        }
        by_type.entry(t).or_default().push(e);
    }
    for (t, n) in live {
        out.insert(keys::count(t), values::encode(&n.into()));
    }
    for (t, of) in &by_type {
        for index in value_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                out.extend(index_entries(r, t, &index, of)?);
            }
        }
        let places: Vec<(u64, String)> = of.iter().map(|e| (e.seq, e.id.clone())).collect();
        for index in text::text_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                out.extend(text::entries(r, t, &index, &places)?);
            }
        }
        let ids: Vec<String> = of.iter().map(|e| e.id.clone()).collect();
        for index in vector::vector_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                out.extend(vector::entries(r, t, &index, &ids)?);
            }
        }
    }
    for (key, bytes) in r.scan(&keys::all_relations())? {
        let relation = match decode_key(&key)?.pop() {
            Some(Part::String(id)) => id,
            _ => return Err(Error::new(ErrorCode::Corrupted, "A relation's key doesn't read")),
        };
        let s: relations::Stored = rmp_serde::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("Relation {relation}'s row doesn't read: {e}")))?;
        let edge = |far: &str| values::encode(&Value::Array(vec![relation.as_str().into(), far.into()]));
        out.insert(keys::edge(true, &s.source, &s.kind, s.seq), edge(&s.target));
        out.insert(keys::edge(false, &s.target, &s.kind, s.seq), edge(&s.source));
        out.insert(keys::between(&s.source, &s.kind, &s.target, s.seq), values::encode(&relation.as_str().into()));
    }
    Ok(out)
}

/// The derived prefixes `derive` answers for, and so `check` compares: all but the index markers, which record what
/// was built rather than following from primary data
const COMPARED: [Prefix; 14] = [
    Prefix::Live,
    Prefix::Trashed,
    Prefix::Count,
    Prefix::Index,
    Prefix::Out,
    Prefix::In,
    Prefix::Between,
    Prefix::TextTerm,
    Prefix::TextPositions,
    Prefix::TextDocument,
    Prefix::TextStats,
    Prefix::TextWord,
    Prefix::VectorChunk,
    Prefix::VectorPending,
];

/// The prefixes whose keys belong to one index of a type, its second and third parts, compared only once it is built
const PER_INDEX: [Prefix; 8] = [
    Prefix::Index,
    Prefix::TextTerm,
    Prefix::TextPositions,
    Prefix::TextDocument,
    Prefix::TextStats,
    Prefix::TextWord,
    Prefix::VectorChunk,
    Prefix::VectorPending,
];

/// Every index of a type that is built or waits to be: its value indexes and its text indexes
pub fn index_names(schema: &Schema, entity_type: &str) -> Vec<String> {
    let mut names: Vec<String> = value_indexes(schema, entity_type).into_iter().map(|i| i.name).collect();
    names.extend(text::text_indexes(schema, entity_type).into_iter().map(|i| i.name));
    names.extend(vector::vector_indexes(schema, entity_type).into_iter().map(|i| i.name));
    names
}

/// What is stored under the compared prefixes, an unbuilt index's entries left out
fn stored(r: &impl Read) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for prefix in COMPARED {
        for (key, value) in r.scan(&prefix.key().to_vec())? {
            if PER_INDEX.contains(&prefix) {
                let parts = decode_key(&key)?;
                let built = match (parts.get(1), parts.get(2)) {
                    (Some(Part::String(t)), Some(Part::String(i))) => is_built(r, t, i)?,
                    _ => false,
                };
                if !built {
                    continue;
                }
            }
            out.insert(key, value);
        }
    }
    Ok(out)
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
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty() && self.violations.is_empty()
    }
}

fn key_text(key: &[u8]) -> String {
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
    let expected = derive(r)?;
    let actual = stored(r)?;
    let mut differences = Vec::new();
    for (key, value) in &expected {
        match actual.get(key) {
            None => differences.push(Difference { key: key_text(key), kind: "missing".into() }),
            Some(v) if v != value => differences.push(Difference { key: key_text(key), kind: "differs".into() }),
            Some(_) => {}
        }
    }
    for key in actual.keys().filter(|k| !expected.contains_key(*k)) {
        differences.push(Difference { key: key_text(key), kind: "extra".into() });
    }
    let mut violations = violations(r)?;
    for message in vector::check_items(r, r.schema())? {
        violations.push(Violation { subject: "vector graph".into(), message });
    }
    Ok(CheckReport { differences, violations })
}

/// Rows that break the schema's rules: a missing required field, a value its field doesn't take, a field its type
/// doesn't declare, a dangling reference, and relations past their kind's cardinality or with a missing end
fn violations(r: &impl Read) -> Result<Vec<Violation>> {
    let schema: &Schema = r.schema();
    let mut out = Vec::new();
    for e in entities_of(r)? {
        let t = ids::type_of(&e.id);
        let Some(def) = schema.types.get(t) else { continue };
        let fields = entities::fields(r, &e.id)?;
        let held: std::collections::HashSet<&str> = fields.iter().map(|(f, _)| f.as_str()).collect();
        for (name, field) in &def.fields {
            if !field.optional && !held.contains(name.as_str()) {
                out.push(Violation { subject: e.id.clone(), message: format!("lacks {name}, which every {t} holds") });
            }
        }
        for (name, bytes) in &fields {
            if entities::field::RESERVED.contains(&name.as_str()) {
                continue;
            }
            let value = values::decode(bytes)?;
            match def.fields.get(name) {
                Some(field) => {
                    if let Err(e2) = values::check_field(t, name, field, &value) {
                        out.push(Violation { subject: e.id.clone(), message: e2.message });
                    }
                    if let crate::schema::ValueType::Ref { .. } = field.value
                        && let Some(target) = value.as_str()
                        && entities::record(r, target)?.is_none()
                    {
                        out.push(Violation {
                            subject: e.id.clone(),
                            message: format!("{name} names {target}, which no entity has"),
                        });
                    }
                }
                None if def.additional_fields == crate::schema::AdditionalFields::Refuse => {
                    out.push(Violation {
                        subject: e.id.clone(),
                        message: format!("holds {name}, which {t} doesn't declare"),
                    });
                }
                None => {}
            }
        }
    }
    // A unique index whose entries name more than one entity: data loaded unchecked can hold such values. Every type
    // with entities has the built-in ones, declared or not
    let mut types: std::collections::BTreeSet<String> = schema.types.keys().cloned().collect();
    for e in entities_of(r)? {
        types.insert(ids::type_of(&e.id).to_owned());
    }
    for entity_type in &types {
        for index in value_indexes(schema, entity_type).into_iter().filter(|i| i.unique) {
            let mut holders: BTreeMap<Vec<u8>, (Vec<Part>, Vec<String>)> = BTreeMap::new();
            for (key, id) in r.scan(&keys::index_range(entity_type, &index.name, &[]))? {
                let mut parts = decode_key(&key)?;
                parts.drain(..3);
                parts.pop();
                let holder = values::decode(&id)?.as_str().unwrap_or_default().to_owned();
                holders.entry(crate::format::key::encode(&parts)).or_insert_with(|| (parts, Vec::new())).1.push(holder);
            }
            for (parts, ids) in holders.into_values().filter(|(_, ids)| ids.len() > 1) {
                out.push(Violation {
                    subject: ids.join(", "),
                    message: format!("hold the same {entity_type}.{} value {parts:?}, which is unique", index.name),
                });
            }
        }
    }
    // Cardinality counts far ends, not edges (`schema.md`, Relations): a second edge of a kind that isn't unique, to
    // the same far end, leaves a target one source, as the write that allowed it found
    let mut targets_of: BTreeMap<(String, String), std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut sources_of: BTreeMap<(String, String), std::collections::BTreeSet<String>> = BTreeMap::new();
    for (key, bytes) in r.scan(&keys::all_relations())? {
        let relation = key_text(&key);
        let s: relations::Stored = rmp_serde::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("A relation's row doesn't read: {e}")))?;
        for end in [&s.source, &s.target] {
            if entities::record(r, end)?.is_none() {
                out.push(Violation { subject: relation.clone(), message: format!("joins {end}, which no entity has") });
            }
        }
        targets_of.entry((s.source.clone(), s.kind.clone())).or_default().insert(s.target.clone());
        sources_of.entry((s.target.clone(), s.kind.clone())).or_default().insert(s.source.clone());
    }
    for (kind, def) in &schema.relations {
        let one_source = matches!(def.cardinality, Cardinality::OneToMany | Cardinality::OneToOne);
        let one_target = matches!(def.cardinality, Cardinality::ManyToOne | Cardinality::OneToOne);
        for ((end, k), far) in &sources_of {
            let n = far.len();
            if k == kind && one_source && n > 1 {
                out.push(Violation {
                    subject: end.clone(),
                    message: format!("has {n} {kind} sources, and a {kind} target has one"),
                });
            }
        }
        for ((end, k), far) in &targets_of {
            let n = far.len();
            if k == kind && one_target && n > 1 {
                out.push(Violation {
                    subject: end.clone(),
                    message: format!("has {n} {kind} targets, and a {kind} source has one"),
                });
            }
        }
    }
    Ok(out)
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
            for (key, _) in t.scan(&keys::index_range(entity_type, name, &[]))? {
                t.del(&key)?;
            }
            text::clear(t, entity_type, name)?;
            t.put(&keys::index_built(entity_type, name), &values::encode(&true.into()))?;
            let all = entities_of(t)?;
            let of: Vec<&Entity> = all.iter().filter(|e| ids::type_of(&e.id) == entity_type).collect();
            let found = value_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name);
            if let Some(found) = found {
                let entries = index_entries(t, entity_type, &found, &of)?;
                buildable(entity_type, &found, &entries)?;
                for (key, value) in entries {
                    t.put(&key, &value)?;
                }
            }
            if let Some(found) = text::text_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) {
                let places: Vec<(u64, String)> = of.iter().map(|e| (e.seq, e.id.clone())).collect();
                for (key, value) in text::entries(t, entity_type, &found, &places)? {
                    t.put(&key, &value)?;
                }
            }
            if let Some(found) = vector::vector_indexes(t.schema, entity_type).into_iter().find(|i| i.name == name) {
                vector::clear(t, entity_type, name)?;
                t.put(&keys::index_built(entity_type, name), &values::encode(&true.into()))?;
                for e in &of {
                    vector::refile_at(t, entity_type, &found, &e.id)?;
                }
                vector::flush(t)?;
            }
        }
        None => {
            for prefix in COMPARED {
                for (key, _) in t.scan(&prefix.key().to_vec())? {
                    t.del(&key)?;
                }
            }
            // Every declared index of every type with entities is built by a full rebuild, but a value index a build
            // couldn't finish, which keeps the entries that fit unmarked, as writes keep an unbuilt index's
            let all = entities_of(t)?;
            let mut by_type: BTreeMap<String, Vec<&Entity>> = BTreeMap::new();
            for e in &all {
                by_type.entry(ids::type_of(&e.id).to_owned()).or_default().push(e);
            }
            for (ty, of) in &by_type {
                let value: Vec<ValueIndex> = value_indexes(t.schema, ty);
                for index in index_names(t.schema, ty) {
                    let marker = keys::index_built(ty, &index);
                    let Some(found) = value.iter().find(|i| i.name == index) else {
                        t.put(&marker, &values::encode(&true.into()))?;
                        continue;
                    };
                    let entries = index_entries(t, ty, found, of)?;
                    match buildable(ty, found, &entries) {
                        Ok(()) => t.put(&marker, &values::encode(&true.into()))?,
                        Err(e) if matches!(e.code, ErrorCode::Unique | ErrorCode::ValueInvalid) => {
                            t.del(&marker)?;
                            for (key, value) in entries.into_iter().filter(|(k, _)| k.len() <= values::MAX_KEY_BYTES) {
                                t.put(&key, &value)?;
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            for (key, value) in derive(t)? {
                t.put(&key, &value)?;
            }
            vector::rebuild_graphs(t)?;
        }
    }
    Ok(())
}

/// Builds one index of a type over the entities after place `after`, at most `limit` of them, answering the last
/// place built or `None` when the whole type is done (and the marker written). Refuses an entry a unique index holds
/// for another entity (`UNIQUE`), which leaves the index unbuilt
pub fn build_chunk(
    t: &mut Txn<'_, '_>,
    entity_type: &str,
    name: &str,
    after: u64,
    limit: usize,
) -> Result<Option<u64>> {
    let mut ranged: Vec<(u64, String)> = Vec::new();
    for trashed in [false, true] {
        for (key, id) in t.scan(&keys::type_range(trashed, entity_type))? {
            let seq = match decode_key(&key)?.pop() {
                Some(Part::Number(n)) => n as u64,
                _ => 0,
            };
            if seq > after {
                ranged.push((seq, values::decode(&id)?.as_str().unwrap_or_default().to_owned()));
            }
        }
    }
    ranged.sort_by_key(|(seq, _)| *seq);
    let chunk: Vec<(u64, String)> = ranged.into_iter().take(limit + 1).collect();
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
                t.put(&keys::index_built(entity_type, name), &values::encode(&true.into()))?;
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
            t.put(&keys::index_built(entity_type, name), &values::encode(&true.into()))?;
            return Ok(None);
        }
        return Ok(Some(last));
    };
    for (seq, id) in chunk.iter().take(limit) {
        let fields: Vec<Option<Value>> = index.fields.iter().map(|f| entities::get(t, id, f)).collect::<Result<_>>()?;
        for parts in entries_of(&fields) {
            if index.unique {
                let mut holder = None;
                t.visit(&keys::index_range(entity_type, name, &parts), |_, v| {
                    holder = values::decode(v).ok().and_then(|v| v.as_str().map(str::to_owned));
                    holder.as_deref() == Some(id.as_str())
                })?;
                if let Some(other) = holder.filter(|h| h != id) {
                    return Err(unbuildable(entity_type, name, &other, id));
                }
            }
            let key = keys::index_entry(entity_type, name, &parts, *seq);
            entities::key_fits(&key, id, entity_type, name)?;
            t.put(&key, &values::encode(&id.as_str().into()))?;
        }
        last = *seq;
    }
    if done {
        t.put(&keys::index_built(entity_type, name), &values::encode(&true.into()))?;
        return Ok(None);
    }
    Ok(Some(last))
}
