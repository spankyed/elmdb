//! Derived data from primary data alone (`docs/architecture/format.md`, Keys): what every derived key should hold, which `check()`
//! compares with what is stored and `rebuild()` writes in its place. An index is read only once built (its `d/xb`
//! marker); writes keep every index whether built or not, so building one fills in what predates it.

mod audit;
mod sorted;
#[cfg(test)]
use audit::{PrimaryGraph, audit_cycles};

use std::collections::BTreeMap;

use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};
use crate::format::Prefix;
use crate::format::key::{Part, decode as decode_key};
use crate::schema::Schema;

use super::entities::{self, ValueIndex, entries_of, value_indexes};
use super::keys;
use super::unit::{Read, Txn};
use super::{ids, relations, text, values, vector};

#[cfg(test)]
pub(crate) fn fail_next_scratch() {
    sorted::FAIL_NEXT_NEW.set(true);
}

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
    derive_into(r, |key, value| {
        out.insert(key, value);
        Ok(())
    })?;
    Ok(out)
}

fn derive_into(r: &impl Read, mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
    let all = entities_of(r)?;
    let mut live: BTreeMap<&str, i64> = BTreeMap::new();
    let mut by_type: BTreeMap<&str, Vec<&Entity>> = BTreeMap::new();
    for e in &all {
        let t = ids::type_of(&e.id);
        emit(keys::of_type(e.trashed, t, e.seq), values::encode(&e.id.as_str().into()))?;
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
                for (key, value) in index_entries(r, t, &index, of)? {
                    emit(key, value)?;
                }
            }
        }
        let places: Vec<(u64, String)> = of.iter().map(|e| (e.seq, e.id.clone())).collect();
        for index in text::text_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                text::entries_into(r, t, &index, &places, &mut emit)?;
            }
        }
        let ids: Vec<String> = of.iter().map(|e| e.id.clone()).collect();
        for index in vector::vector_indexes(r.schema(), t) {
            if is_built(r, t, &index.name)? {
                for (key, value) in vector::entries(r, t, &index, &ids)? {
                    emit(key, value)?;
                }
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
        emit(keys::edge(true, &s.source, &s.kind, s.seq), edge(&s.target))?;
        emit(keys::edge(false, &s.target, &s.kind, s.seq), edge(&s.source))?;
        emit(keys::between(&s.source, &s.kind, &s.target, s.seq), values::encode(&relation.as_str().into()))?;
    }
    Ok(())
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

/// Whether a stored derived key participates in comparison; unbuilt index contents are intentionally ignored.
fn compared(r: &impl Read, prefix: Prefix, key: &[u8]) -> Result<bool> {
    if !PER_INDEX.contains(&prefix) {
        return Ok(true);
    }
    let parts = decode_key(key)?;
    match (parts.get(1), parts.get(2)) {
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
                } else {
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
    Ok(CheckReport { differences, violations })
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
            t.clear_prefix(&keys::index_range(entity_type, name, &[]))?;
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
                t.clear_prefix(&prefix.key().to_vec())?;
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
            let mut scratch = sorted::Scratch::new()?;
            derive_into(t, |key, value| scratch.push(key, value))?;
            let mut entries = scratch.finish()?;
            while let Some((key, value)) = entries.next()? {
                t.put(&key, &value)?;
            }
            vector::rebuild_graphs(t)?;
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

#[cfg(test)]
mod audit_tests {
    use super::*;

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
                        println!("rows={size}, cursor_entries={visited}, chunk=64");
                        Ok(())
                    },
                    false,
                )
                .unwrap();
        }
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
