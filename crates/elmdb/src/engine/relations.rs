//! Relations in the store's keys (`docs/architecture/format.md`, Keys):
//!
//! - `p/r relation` → `{ kind, source, target, info, seq }`: the relation's row and its place
//! - `d/o source kind seq` → `[relation, target]` and `d/n target kind seq` → `[relation, source]`: each entity's edges
//!   out and in, by kind, in the order linked, so a walk reads a far end without the row
//! - `d/b source kind target seq` → relation: the edges between two ends, so a link finds an edge it would repeat
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

fn file(t: &mut Txn<'_, '_>, relation: &str, s: &Stored) -> Result<()> {
    let edge = |far: &str| values::encode(&Value::Array(vec![relation.into(), far.into()]));
    t.put(&keys::edge(true, &s.source, &s.kind, s.seq), &edge(&s.target))?;
    t.put(&keys::edge(false, &s.target, &s.kind, s.seq), &edge(&s.source))?;
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
    let pair = value.as_array().filter(|a| a.len() == 2);
    match pair.map(|a| (a[0].as_str(), a[1].as_str())) {
        Some((Some(relation), Some(far))) => Ok((relation.to_owned(), far.to_owned())),
        _ => Err(Error::new(ErrorCode::Corrupted, "An edge key's value doesn't read")),
    }
}

/// The entity's edges out (or in), of one kind or every kind, in the order linked: each relation and far end
pub fn edges(t: &impl Read, entity: &str, outward: bool, kind: Option<&str>) -> Result<Vec<(String, String)>> {
    t.scan(&keys::edges_of(outward, entity, kind))?.into_iter().map(|(_, v)| edge_of(&v)).collect()
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

/// Whether linking `source` to `target` would close a cycle through `kind`: `target` already reaches `source`. Every
/// edge counts, a trashed entity's included, since restoring it would otherwise close the cycle
fn would_cycle(t: &impl Read, source: &str, target: &str, kind: &str) -> Result<bool> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![target.to_owned()];
    while let Some(node) = stack.pop() {
        if node == source {
            return Ok(true);
        }
        for (_, far) in edges(t, &node, true, Some(kind))? {
            if seen.insert(far.clone()) {
                stack.push(far);
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
    if def.acyclic && would_cycle(t, source, target, kind)? {
        return Err(Error::new(ErrorCode::Cycle, format!("Cannot create a {kind} relation that would form a cycle")));
    }
    Ok(())
}

fn check_info(def: &RelationDef, kind: &str, info: Option<&Value>) -> Result<()> {
    match info {
        Some(info) => values::check_field("relation", kind, &def.info, info),
        None if def.info.optional => Ok(()),
        None => Err(Error::new(ErrorCode::RequiredField, format!("A {kind} relation's info is required"))),
    }
}

/// Links `source` to `target`, returning the relation and whether it was linked now. Idempotent on the whole edge:
/// with no `info`, any edge between the ends is the edge; with one, an edge holding the same info. A `unique` kind
/// keeps one edge per pair, whose info a link replaces
pub fn link(
    t: &mut Txn<'_, '_>,
    source: &str,
    kind: &str,
    target: &str,
    info: Option<Value>,
) -> Result<(String, bool)> {
    let def = definition(t, kind)?.clone();
    if let Some(info) = &info {
        values::check_spec(info)?;
    }
    check_info(&def, kind, info.as_ref())?;
    let existing = between(t, source, kind, target)?;
    if def.unique
        && let Some(relation) = existing.first()
    {
        if let Some(info) = info {
            let mut stored = get(t, relation)?.expect("an edge's relation exists");
            if stored.info.as_ref().is_none_or(|i| !values::same(i, &info)) {
                stored.info = Some(info);
                put(t, relation, &stored)?;
                t.event(Event::RelationChanged {
                    relation: relation.clone(),
                    kind: kind.to_owned(),
                    source: source.to_owned(),
                    target: target.to_owned(),
                });
            }
        }
        return Ok((relation.clone(), false));
    }
    for relation in &existing {
        let held = get(t, relation)?.expect("an edge's relation exists");
        let matches = match (&info, &held.info) {
            (None, _) => true,
            (Some(a), Some(b)) => values::same(a, b),
            (Some(_), None) => false,
        };
        if matches {
            return Ok((relation.clone(), false));
        }
    }
    check_rules(t, &def, kind, source, target, None)?;
    let seq = entities::next_seq(t)?;
    let relation = ids::new_relation_id(t.now, seq);
    let stored = Stored { kind: kind.to_owned(), source: source.to_owned(), target: target.to_owned(), info, seq };
    put(t, &relation, &stored)?;
    file(t, &relation, &stored)?;
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

/// Moves the relation to new ends or replaces its info, keeping its id; a move files it last among its new ends' edges
pub fn update(
    t: &mut Txn<'_, '_>,
    relation: &str,
    source: Option<&str>,
    target: Option<&str>,
    info: Option<Value>,
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
    let moved = next.source != before.source || next.target != before.target;
    if moved {
        check_rules(t, &def, &next.kind, &next.source, &next.target, Some(relation))?;
        unfile(t, &before)?;
        next.seq = entities::next_seq(t)?;
        file(t, relation, &next)?;
        t.defer(Check::Ends { relation: relation.to_owned(), via: "updateRelation" });
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
