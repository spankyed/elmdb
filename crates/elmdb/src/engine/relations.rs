//! Relations in the store's keys (`docs/architecture/format.md`, Keys):
//!
//! - `p/r relation` → `{ kind, source, target, info, weight, seq }`: the relation's row and its place, `weight` held
//!   only when it isn't 1 (a kind declared `weighted`)
//! - `d/o source kind seq` → `[relation, target]` and `d/n target kind seq` → `[relation, source]`: each entity's edges
//!   out and in, by kind, in the order linked, so a walk reads a far end without the row; a third element, the weight,
//!   when the row holds one, so a weighted graph reads it there too
//! - `d/b source kind target seq` → relation: the edges between two ends, so a link finds the edge with its identity
//!
//! A relation is no entity: it holds no fields and no type lists it. The schema's rules for its kind are kept here.

use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};
use crate::schema::{Cardinality, RelationDef};

use super::changes::Event;
use super::entities;
use super::keys;
use super::unit::{Check, Read, Txn};
use super::{ids, values};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    pub kind: String,
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub info: Option<Value>,
    /// The edge's weight when it isn't 1, which only a kind declared `weighted` holds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    pub seq: u64,
}

pub fn get(t: &impl Read, relation: &str) -> Result<Option<Stored>> {
    let Some(bytes) = t.get(&keys::relation(relation))? else { return Ok(None) };
    rmp_serde::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Error::new(ErrorCode::Corrupted, format!("Relation {relation}'s row doesn't read: {e}")))
}

fn put(t: &mut Txn<'_, '_>, relation: &str, stored: &Stored) -> Result<()> {
    let bytes = rmp_serde::to_vec_named(stored).expect("a relation row serializes");
    t.put(&keys::relation(relation), &bytes)
}

/// An edge key's value: `[relation, far]`, and the row's weight third when it holds one. Filing and `rebuild` both
/// write it through here, so `check` compares the same bytes
pub fn edge_value(relation: &str, far: &str, weight: Option<f64>) -> Vec<u8> {
    let mut parts = vec![relation.into(), far.into()];
    parts.extend(weight.map(Value::F64));
    values::encode(&Value::Array(parts))
}

/// The largest weight an edge or a seed takes, 2^960: a graph holds fewer than 2^32 edges and a plan fewer than 2^32
/// seeds, so no sum of weights a rank adds up can reach infinity, which would lose the walkers it holds
pub const MAX_WEIGHT: f64 = f64::from_bits((1023 + 960) << 52);

/// The edge entries out and in, which hold the weight; the entry between the ends doesn't, so a new weight on edges
/// that stay where they are is written here alone
fn file_edges(t: &mut Txn<'_, '_>, relation: &str, s: &Stored) -> Result<()> {
    t.put(&keys::edge(true, &s.source, &s.kind, s.seq), &edge_value(relation, &s.target, s.weight))?;
    t.put(&keys::edge(false, &s.target, &s.kind, s.seq), &edge_value(relation, &s.source, s.weight))
}

fn file(t: &mut Txn<'_, '_>, relation: &str, s: &Stored) -> Result<()> {
    file_edges(t, relation, s)?;
    t.put(&keys::between(&s.source, &s.kind, &s.target, s.seq), &values::encode(&relation.into()))
}

fn unfile(t: &mut Txn<'_, '_>, s: &Stored) -> Result<()> {
    t.del(&keys::edge(true, &s.source, &s.kind, s.seq))?;
    t.del(&keys::edge(false, &s.target, &s.kind, s.seq))?;
    t.del(&keys::between(&s.source, &s.kind, &s.target, s.seq))
}

/// An edge key's value: the relation and the far end
pub fn edge_of(bytes: &[u8]) -> Result<(String, String)> {
    let value = values::decode(bytes)?;
    let pair = value.as_array().filter(|a| a.len() == 2 || a.len() == 3);
    match pair.map(|a| (a[0].as_str(), a[1].as_str())) {
        Some((Some(relation), Some(far))) => Ok((relation.to_owned(), far.to_owned())),
        _ => Err(Error::new(ErrorCode::Corrupted, "An edge key's value doesn't read")),
    }
}

/// The entity's edges out (or in), of one kind or every kind, in the order linked: each relation and far end
pub fn edges(t: &impl Read, entity: &str, outward: bool, kind: Option<&str>) -> Result<Vec<(String, String)>> {
    t.scan(&keys::edges_of(outward, entity, kind))?.into_iter().map(|(_, v)| edge_of(&v)).collect()
}

/// Visits an entity's edges out or in, of one kind or of every kind, in place order or (`backward`) from the last,
/// until `each` answers false: each edge's place and far end. It reads no edge past the one `each` stopped at
pub fn visit_edges(
    t: &impl Read,
    entity: &str,
    outward: bool,
    kind: Option<&str>,
    backward: bool,
    mut each: impl FnMut(u64, String) -> Result<bool>,
) -> Result<()> {
    let mut result = Ok(true);
    let on = |key: &[u8], value: &[u8]| {
        result = place_of(key).and_then(|place| each(place, edge_of(value)?.1));
        matches!(result, Ok(true))
    };
    let prefix = keys::edges_of(outward, entity, kind);
    if backward {
        t.visit_back(&prefix, on)?
    } else {
        t.visit(&prefix, on)?
    }
    result.map(|_| ())
}

/// An edge key's far end and weight, read where they lie: an edge's value is plain msgpack, `[relation, far]` or
/// `[relation, far, weight]`
fn far_of(bytes: &[u8]) -> Result<(&str, Option<f64>)> {
    let corrupt = || Error::new(ErrorCode::Corrupted, "An edge key's value doesn't read");
    let mut rest = bytes;
    let len = rmp::decode::read_array_len(&mut rest).map_err(|_| corrupt())?;
    if len != 2 && len != 3 {
        return Err(corrupt());
    }
    let (_, rest) = rmp::decode::read_str_from_slice(rest).map_err(|_| corrupt())?;
    let (far, mut rest) = rmp::decode::read_str_from_slice(rest).map_err(|_| corrupt())?;
    let weight = if len == 3 { Some(rmp::decode::read_f64(&mut rest).map_err(|_| corrupt())?) } else { None };
    Ok((far, weight))
}

/// Visits the far end of each of an entity's edges out (or in), of one kind or every kind, in place order, until
/// `each` answers false, with the edge's weight when it holds one; a far end is lent, not copied, so a caller that only
/// looks one up allocates nothing
pub fn visit_far_ends(
    t: &impl Read,
    entity: &str,
    outward: bool,
    kind: Option<&str>,
    mut each: impl FnMut(&str, Option<f64>) -> Result<bool>,
) -> Result<()> {
    let mut result = Ok(true);
    t.visit(&keys::edges_of(outward, entity, kind), |_, value| {
        result = far_of(value).and_then(|(far, weight)| each(far, weight));
        matches!(result, Ok(true))
    })?;
    result.map(|_| ())
}

/// The place of the first edge of a kind from `source` to `target`, the place order of its edges being their order
pub fn first_between(t: &impl Read, source: &str, kind: &str, target: &str) -> Result<Option<u64>> {
    let mut first = Ok(None);
    t.visit(&keys::between_range(source, kind, target), |key, _| {
        first = place_of(key).map(Some);
        false
    })?;
    first
}

/// The place an edge's or a between entry's key ends with
fn place_of(key: &[u8]) -> Result<u64> {
    match crate::format::key::decode(key)?.pop() {
        Some(crate::format::key::Part::Number(n)) => Ok(n as u64),
        _ => Err(Error::new(ErrorCode::Corrupted, "An edge key's place doesn't read")),
    }
}

/// The relations of a kind between two ends, in the order linked
pub fn between(t: &impl Read, source: &str, kind: &str, target: &str) -> Result<Vec<String>> {
    t.scan(&keys::between_range(source, kind, target))?
        .into_iter()
        .map(|(_, v)| values::decode(&v).map(|v| v.as_str().unwrap_or_default().to_owned()))
        .collect()
}

pub fn definition<'s>(t: &'s impl Read, kind: &str) -> Result<&'s RelationDef> {
    t.schema().relations.get(kind).ok_or_else(|| {
        Error::new(ErrorCode::UnknownRelationKind, format!("\"{kind}\" isn't a relation kind the schema declares"))
    })
}

/// Whether linking `source` to `target` would close a cycle through `kinds`: `target` already reaches `source` over
/// edges of any of them. Every edge counts, a trashed entity's included, since restoring it would otherwise close the
/// cycle. A link checks its own kind; the `wouldCreateCycle` read asks the same of the kinds it names
pub fn would_cycle(t: &impl Read, source: &str, target: &str, kinds: &[String]) -> Result<bool> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![target.to_owned()];
    while let Some(node) = stack.pop() {
        if node == source {
            return Ok(true);
        }
        for kind in kinds {
            for (_, far) in edges(t, &node, true, Some(kind))? {
                if seen.insert(far.clone()) {
                    stack.push(far);
                }
            }
        }
    }
    Ok(false)
}

/// Refuses an edge of `def` from `source` to `target` its kind's rules don't allow, an edge `relation` itself (when
/// it is being moved) aside
fn check_rules(
    t: &impl Read,
    def: &RelationDef,
    kind: &str,
    source: &str,
    target: &str,
    relation: Option<&str>,
) -> Result<()> {
    if source == target {
        return Err(Error::new(ErrorCode::SelfLink, format!("{source} can't be linked to itself ({kind})")));
    }
    if let Some(endpoints) = &def.endpoints {
        let (s, g) = (ids::type_of(source), ids::type_of(target));
        if !endpoints.iter().any(|e| e.source == s && e.target == g) {
            return Err(Error::new(ErrorCode::Endpoint, format!("A {kind} relation doesn't join a {s} to a {g}")));
        }
    }
    let other = |(rel, far): &(String, String), end: &str| Some(rel.as_str()) != relation && far != end;
    let one_source = matches!(def.cardinality, Cardinality::OneToMany | Cardinality::OneToOne);
    let one_target = matches!(def.cardinality, Cardinality::ManyToOne | Cardinality::OneToOne);
    if one_source && edges(t, target, false, Some(kind))?.iter().any(|e| other(e, source)) {
        return Err(Error::new(
            ErrorCode::Cardinality,
            format!("{target} already has a {kind} source, and a {kind} target has one; unlink it first"),
        ));
    }
    if one_target && edges(t, source, true, Some(kind))?.iter().any(|e| other(e, target)) {
        return Err(Error::new(
            ErrorCode::Cardinality,
            format!("{source} already has a {kind} target, and a {kind} source has one; unlink it first"),
        ));
    }
    if def.acyclic && would_cycle(t, source, target, &[kind.to_owned()])? {
        return Err(Error::new(ErrorCode::Cycle, format!("Cannot create a {kind} relation that would form a cycle")));
    }
    Ok(())
}

/// A weight given to an edge of `def`, as the row holds it: refused unless the kind is declared weighted and the weight
/// is a number from 0 to `MAX_WEIGHT`; 1, every edge's weight unless given another, held as none, and -0 held as 0, so
/// equal weights are the same bytes
fn weight_of(def: &RelationDef, kind: &str, weight: Option<f64>) -> Result<Option<f64>> {
    let Some(weight) = weight else { return Ok(None) };
    if !def.weighted {
        return Err(Error::new(
            ErrorCode::ValueInvalid,
            format!("A {kind} relation carries no weight: its kind isn't declared weighted"),
        ));
    }
    if !(0.0..=MAX_WEIGHT).contains(&weight) {
        return Err(Error::new(
            ErrorCode::ValueInvalid,
            format!("A {kind} relation's weight is a number from 0 to 2^960, and was given {weight}"),
        ));
    }
    Ok((weight != 1.0).then_some(weight.abs()))
}

fn check_info(def: &RelationDef, kind: &str, info: Option<&Value>) -> Result<()> {
    match info {
        Some(info) => values::check_field("relation", kind, &def.info, info),
        None if def.info.optional => Ok(()),
        None => Err(Error::new(ErrorCode::RequiredField, format!("A {kind} relation's info is required"))),
    }
}

/// The values of the kind's key in an edge's info, a field the info doesn't hold (or info that is no map) read as null:
/// with the edge's two ends, what identifies it
pub fn key_values(def: &RelationDef, info: Option<&Value>) -> Vec<Value> {
    let entries = match info {
        Some(Value::Map(entries)) => entries.as_slice(),
        _ => &[],
    };
    def.key
        .iter()
        .map(|field| entries.iter().find(|(k, _)| k.as_str() == Some(field)).map_or(Value::Nil, |(_, v)| v.clone()))
        .collect()
}

fn same_key(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values::same(x, y))
}

/// The edge of `kind` from `source` to `target` holding `key` other than `except`: the one a link to that identity
/// updates, and the one a move onto it would duplicate
fn with_identity(
    t: &impl Read,
    def: &RelationDef,
    source: &str,
    kind: &str,
    target: &str,
    key: &[Value],
    except: Option<&str>,
) -> Result<Option<String>> {
    for relation in between(t, source, kind, target)? {
        if except == Some(relation.as_str()) {
            continue;
        }
        if def.key.is_empty() {
            return Ok(Some(relation));
        }
        let held = get(t, &relation)?.ok_or_else(|| missing_relation(&relation))?;
        if same_key(&key_values(def, held.info.as_ref()), key) {
            return Ok(Some(relation));
        }
    }
    Ok(None)
}

/// Links `source` to `target`, returning the relation and whether it was linked now. An edge is identified by its
/// ends and the values of its kind's key (`schema.md`, Relations): a link to an identity an edge holds updates that
/// edge, replacing its info with any given and its weight with any given, and keeps its id and its place
pub fn link(
    t: &mut Txn<'_, '_>,
    source: &str,
    kind: &str,
    target: &str,
    info: Option<Value>,
    weight: Option<f64>,
) -> Result<(String, bool)> {
    let def = definition(t, kind)?.clone();
    if let Some(info) = &info {
        values::check_spec(info)?;
    }
    check_info(&def, kind, info.as_ref())?;
    let held_weight = weight_of(&def, kind, weight)?;
    let key = key_values(&def, info.as_ref());
    if let Some(relation) = with_identity(t, &def, source, kind, target, &key, None)? {
        let before = get(t, &relation)?.ok_or_else(|| missing_relation(&relation))?;
        let mut stored = before.clone();
        if let Some(info) = info
            && stored.info.as_ref().is_none_or(|i| !values::same(i, &info))
        {
            stored.info = Some(info);
        }
        if weight.is_some() {
            stored.weight = held_weight;
        }
        if stored != before {
            if stored.weight != before.weight {
                file_edges(t, &relation, &stored)?;
                super::ranks::edge_changed(t, kind, source, target, Some(before.weight), Some(stored.weight))?;
            }
            put(t, &relation, &stored)?;
            t.event(Event::RelationChanged {
                relation: relation.clone(),
                kind: kind.to_owned(),
                source: source.to_owned(),
                target: target.to_owned(),
            });
        }
        return Ok((relation, false));
    }
    check_rules(t, &def, kind, source, target, None)?;
    let seq = entities::next_seq(t)?;
    let relation = ids::new_relation_id(t.now, seq);
    let stored = Stored {
        kind: kind.to_owned(),
        source: source.to_owned(),
        target: target.to_owned(),
        info,
        weight: held_weight,
        seq,
    };
    put(t, &relation, &stored)?;
    file(t, &relation, &stored)?;
    super::ranks::edge_changed(t, kind, source, target, None, Some(stored.weight))?;
    t.event(Event::Linked {
        relation: relation.clone(),
        kind: kind.to_owned(),
        source: source.to_owned(),
        target: target.to_owned(),
    });
    // A relation has two ends when it commits; a unit may link to an entity it creates later
    t.defer(Check::Ends { relation: relation.clone(), via: "link" });
    Ok((relation, true))
}

/// Stores a relation under its own id, as it was, and files its edges: a relation an admin operation moves
pub fn restore(t: &mut Txn<'_, '_>, relation: &str, stored: &Stored) -> Result<()> {
    put(t, relation, stored)?;
    file(t, relation, stored)?;
    super::ranks::edge_changed(t, &stored.kind, &stored.source, &stored.target, None, Some(stored.weight))?;
    t.event(Event::Linked {
        relation: relation.to_owned(),
        kind: stored.kind.clone(),
        source: stored.source.clone(),
        target: stored.target.clone(),
    });
    Ok(())
}

/// Removes the relation; an id no relation has is ignored
pub fn remove(t: &mut Txn<'_, '_>, relation: &str) -> Result<()> {
    let Some(stored) = get(t, relation)? else { return Ok(()) };
    unfile(t, &stored)?;
    super::ranks::edge_changed(t, &stored.kind, &stored.source, &stored.target, Some(stored.weight), None)?;
    t.del(&keys::relation(relation))?;
    t.event(Event::Unlinked {
        relation: relation.to_owned(),
        kind: stored.kind,
        source: stored.source,
        target: stored.target,
    });
    Ok(())
}

/// Removes the source's edges of `kind` (or every kind) to `target` (or every target)
pub fn unlink(t: &mut Txn<'_, '_>, source: &str, kind: Option<&str>, target: Option<&str>) -> Result<()> {
    if let Some(kind) = kind {
        definition(t, kind)?;
    }
    let doomed: Vec<String> = match (kind, target) {
        (Some(kind), Some(target)) => between(t, source, kind, target)?,
        _ => edges(t, source, true, kind)?
            .into_iter()
            .filter(|(_, far)| target.is_none_or(|g| far == g))
            .map(|(relation, _)| relation)
            .collect(),
    };
    for relation in doomed {
        remove(t, &relation)?;
    }
    Ok(())
}

/// Moves the relation to new ends, or replaces its info or its weight, keeping its id; a move files it last among its new
/// ends' edges, and a new weight files it again where it was
pub fn update(
    t: &mut Txn<'_, '_>,
    relation: &str,
    source: Option<&str>,
    target: Option<&str>,
    info: Option<Value>,
    weight: Option<f64>,
) -> Result<()> {
    let Some(before) = get(t, relation)? else {
        return Err(Error::new(ErrorCode::NoSuchRelation, format!("No relation has the id {relation}")));
    };
    let def = definition(t, &before.kind)?.clone();
    if let Some(info) = &info {
        values::check_spec(info)?;
        check_info(&def, &before.kind, Some(info))?;
    }
    let mut next = before.clone();
    if let Some(s) = source {
        next.source = s.to_owned();
    }
    if let Some(g) = target {
        next.target = g.to_owned();
    }
    if info.is_some() {
        next.info = info;
    }
    if weight.is_some() {
        next.weight = weight_of(&def, &before.kind, weight)?;
    }
    let moved = next.source != before.source || next.target != before.target;
    let key = key_values(&def, next.info.as_ref());
    if (moved || !same_key(&key, &key_values(&def, before.info.as_ref())))
        && with_identity(t, &def, &next.source, &next.kind, &next.target, &key, Some(relation))?.is_some()
    {
        // Two edges with one identity would leave a link to it no single edge to update
        let identity =
            if def.key.is_empty() { String::new() } else { format!(" with the same {}", def.key.join(", ")) };
        return Err(Error::new(
            ErrorCode::DuplicateEdge,
            format!(
                "updateRelation({relation:?}): a {} edge{identity} already joins {} to {}, and an edge's ends and key \
                 identify it; remove one of them first",
                next.kind, next.source, next.target
            ),
        ));
    }
    if moved {
        check_rules(t, &def, &next.kind, &next.source, &next.target, Some(relation))?;
        unfile(t, &before)?;
        super::ranks::edge_changed(t, &before.kind, &before.source, &before.target, Some(before.weight), None)?;
        next.seq = entities::next_seq(t)?;
        file(t, relation, &next)?;
        super::ranks::edge_changed(t, &next.kind, &next.source, &next.target, None, Some(next.weight))?;
        t.defer(Check::Ends { relation: relation.to_owned(), via: "updateRelation" });
    } else if next.weight != before.weight {
        file_edges(t, relation, &next)?;
        super::ranks::edge_changed(t, &next.kind, &next.source, &next.target, Some(before.weight), Some(next.weight))?;
    }
    put(t, relation, &next)?;
    t.event(Event::RelationChanged {
        relation: relation.to_owned(),
        kind: next.kind,
        source: next.source,
        target: next.target,
    });
    Ok(())
}

/// Removes every relation to or from the entity: one being destroyed
pub fn remove_all_of(t: &mut Txn<'_, '_>, entity: &str) -> Result<()> {
    let mut doomed: Vec<String> = Vec::new();
    for outward in [true, false] {
        for (relation, _) in edges(t, entity, outward, None)? {
            if !doomed.contains(&relation) {
                doomed.push(relation);
            }
        }
    }
    for relation in doomed {
        remove(t, &relation)?;
    }
    Ok(())
}

/// The deferred check of a relation's ends
pub fn check_ends(t: &impl Read, relation: &str, via: &str) -> Result<()> {
    let Some(stored) = get(t, relation)? else { return Ok(()) };
    for end in [&stored.source, &stored.target] {
        if entities::record(t, end)?.is_none() {
            let message = match via {
                "link" => format!("link({}, {:?}): no entity has this id", stored.kind, stored.target),
                _ => format!("{via}({relation:?}): no entity has the id {end:?}"),
            };
            return Err(Error::new(ErrorCode::NoSuchEntity, message));
        }
    }
    Ok(())
}

/// An edge whose relation row is gone: damage `check()` reports, answered as such rather than as a panic
fn missing_relation(relation: &str) -> Error {
    Error::new(ErrorCode::Corrupted, format!("An edge names relation {relation}, which has no row"))
}
