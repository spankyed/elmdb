//! The admin operations a migration runs over stored data (`docs/architecture/schema.md`, Changing a schema): deleting what a removed
//! declaration left (`purge`), and renaming, which a schema change never infers. Each runs in one unit.

use rmpv::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::schema::ValueType;

use super::entities::{self, field};
use super::keys;
use super::unit::{Read, Txn};
use super::{derive, relations, values};

/// What a purge deletes: a type's entities, a field of a type, a relation kind's relations, or a role
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purge {
    Type(String),
    Field { entity_type: String, field: String },
    RelationKind(String),
    Role(String),
}

/// The ids of a type's entities, live and trashed
fn ids_of_type(t: &impl Read, entity_type: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for trashed in [false, true] {
        for (_, id) in t.scan(&keys::type_range(trashed, entity_type))? {
            out.push(values::decode(&id)?.as_str().unwrap_or_default().to_owned());
        }
    }
    Ok(out)
}

/// Every relation's id, with its kind
fn all_relations(t: &impl Read) -> Result<Vec<(String, relations::Stored)>> {
    let mut out = Vec::new();
    for (key, bytes) in t.scan(&keys::all_relations())? {
        let id = match decode_key(&key)?.pop() {
            Some(Part::String(id)) => id,
            _ => continue,
        };
        let stored: relations::Stored = rmp_serde::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("Relation {id}'s row doesn't read: {e}")))?;
        out.push((id, stored));
    }
    Ok(out)
}

fn still_declared(what: &str) -> Error {
    Error::new(
        ErrorCode::SchemaChangeRefused,
        format!("{what} is still declared: purge deletes only what a removed declaration left"),
    )
}

pub fn purge(t: &mut Txn<'_, '_>, what: &Purge) -> Result<()> {
    match what {
        Purge::Type(entity_type) => {
            if t.schema.types.contains_key(entity_type) {
                return Err(still_declared(&format!("The type {entity_type}")));
            }
            for id in ids_of_type(t, entity_type)? {
                relations::remove_all_of(t, &id)?;
                entities::destroy(t, &id)?;
            }
        }
        Purge::Field { entity_type, field: name } => {
            if t.schema.types.get(entity_type).is_some_and(|d| d.fields.contains_key(name)) {
                return Err(still_declared(&format!("{entity_type}.{name}")));
            }
            for id in ids_of_type(t, entity_type)? {
                entities::delete_field(t, &id, name)?;
            }
        }
        Purge::RelationKind(kind) => {
            if t.schema.relations.contains_key(kind) {
                return Err(still_declared(&format!("The relation kind {kind}")));
            }
            for (id, stored) in all_relations(t)? {
                if stored.kind == *kind {
                    relations::remove(t, &id)?;
                }
            }
        }
        Purge::Role(role) => {
            if t.schema.roles.contains_key(role) {
                return Err(still_declared(&format!("The role {role}")));
            }
            for entity_type in t.schema.types.keys().cloned().collect::<Vec<_>>() {
                for id in ids_of_type(t, &entity_type)? {
                    if let Some(Value::Array(roles)) = entities::get(t, &id, field::ROLE)?
                        && roles.iter().any(|r| r.as_str() == Some(role))
                    {
                        let rest: Vec<Value> = roles.into_iter().filter(|r| r.as_str() != Some(role)).collect();
                        if rest.is_empty() {
                            entities::delete_field(t, &id, field::ROLE)?;
                        } else {
                            entities::put_field(t, &id, field::ROLE, &Value::Array(rest))?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn requires_declared(declared: bool, what: &str) -> Result<()> {
    if declared {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::SchemaChangeRefused,
            format!("{what} isn't declared: declare the new name, then rename to it"),
        ))
    }
}

/// Moves a field's values on every entity of a type to another name the type declares
pub fn rename_field(t: &mut Txn<'_, '_>, entity_type: &str, from: &str, to: &str) -> Result<()> {
    requires_declared(
        t.schema.types.get(entity_type).is_some_and(|d| d.fields.contains_key(to)),
        &format!("{entity_type}.{to}"),
    )?;
    for id in ids_of_type(t, entity_type)? {
        if let Some(value) = entities::get(t, &id, from)? {
            entities::delete_field(t, &id, from)?;
            entities::put_field(t, &id, to, &value)?;
        }
    }
    Ok(())
}

/// Moves every relation of a kind to another kind the schema declares, keeping their ids and ends
pub fn rename_relation(t: &mut Txn<'_, '_>, from: &str, to: &str) -> Result<()> {
    requires_declared(t.schema.relations.contains_key(to), &format!("The relation kind {to}"))?;
    for (id, stored) in all_relations(t)? {
        if stored.kind == from {
            relations::remove(t, &id)?;
            let moved = relations::Stored { kind: to.to_owned(), ..stored };
            relations::restore(t, &id, &moved)?;
        }
    }
    Ok(())
}

/// Renames a type: every entity's id from `<from>-…` to `<to>-…`, and with it the relations' ends, the declared `ref`
/// fields that named it, and the type's sequences. A string field holding an id that isn't a declared `ref` isn't
/// rewritten. Derived data is rebuilt from what this leaves
pub fn rename_type(t: &mut Txn<'_, '_>, from: &str, to: &str) -> Result<()> {
    requires_declared(t.schema.types.contains_key(to), &format!("The type {to}"))?;
    let moved: Vec<(String, String)> = ids_of_type(t, from)?
        .into_iter()
        .map(|old| {
            let rest = old.strip_prefix(from).unwrap_or(&old).to_owned();
            (old, format!("{to}{rest}"))
        })
        .collect();
    if moved.is_empty() {
        return Ok(());
    }
    let renamed = |id: &str| moved.iter().find(|(old, _)| old == id).map(|(_, new)| new.clone());
    for (old, new) in &moved {
        if entities::record(t, new)?.is_some() {
            return Err(Error::new(ErrorCode::IdInUse, format!("renameType: {new} exists already")));
        }
        let record = t.get(&keys::entity(old))?.ok_or_else(|| {
            Error::new(ErrorCode::Corrupted, format!("renameType: {old} is listed and has no record"))
        })?;
        t.put(&keys::entity(new), &record)?;
        t.del(&keys::entity(old))?;
        for (name, bytes) in entities::fields(t, old)? {
            t.put(&keys::value(new, &name), &bytes)?;
            t.del(&keys::value(old, &name))?;
        }
        // Its embeddings are primary data, keyed by type and id, under each vector index the type keeps
        for index in super::vector::vector_indexes(t.schema, to) {
            for (key, bytes) in t.scan(&keys::vectors_of(from, &index.name, old))? {
                let Some(Part::Number(chunk)) = decode_key(&key)?.pop() else {
                    return Err(Error::new(ErrorCode::Corrupted, format!("renameType: an embedding key of {old}")));
                };
                t.put(&keys::vector(to, &index.name, new, chunk as u32), &bytes)?;
                t.del(&key)?;
            }
        }
    }
    for (id, stored) in all_relations(t)? {
        let source = renamed(&stored.source);
        let target = renamed(&stored.target);
        if source.is_some() || target.is_some() {
            let next = relations::Stored {
                source: source.unwrap_or(stored.source.clone()),
                target: target.unwrap_or(stored.target.clone()),
                ..stored
            };
            let bytes = rmp_serde::to_vec_named(&next).expect("a relation row serializes");
            t.put(&keys::relation(&id), &bytes)?;
        }
    }
    // The declared `ref` fields that may name an entity of the type
    let refs: Vec<(String, String)> = t
        .schema
        .types
        .iter()
        .flat_map(|(ty, def)| {
            def.fields.iter().filter_map(move |(name, f)| {
                let names = |v: &ValueType| matches!(v, ValueType::Ref { to: targets } if targets.iter().any(|x| x == to || x == from));
                let holds = names(&f.value) || matches!(&f.value, ValueType::List { of } if names(of));
                holds.then(|| (ty.clone(), name.clone()))
            })
        })
        .collect();
    // The renamed type's own entities are under their new ids now, which the type's listing, derived and rebuilt last,
    // doesn't hold yet
    let new_ids: Vec<String> = moved.iter().map(|(_, new)| new.clone()).collect();
    for (ty, name) in refs {
        let ids = if ty == to { new_ids.clone() } else { ids_of_type(t, &ty)? };
        for id in ids {
            let Some(value) = entities::get(t, &id, &name)? else { continue };
            let rewrite = |v: &Value| v.as_str().and_then(renamed).map(Value::from).unwrap_or_else(|| v.clone());
            let next = match &value {
                Value::Array(items) => Value::Array(items.iter().map(rewrite).collect()),
                v => rewrite(v),
            };
            if next != value {
                t.put(&keys::value(&id, &name), &values::encode(&next))?;
            }
        }
    }
    for (key, bytes) in t.scan(&crate::format::Prefix::Sequence.key().to_vec())? {
        if let Some(Part::String(name)) = decode_key(&key)?.pop() {
            let next = name
                .strip_prefix(&format!("shortCode:{from}"))
                .filter(|rest| rest.is_empty())
                .map(|_| format!("shortCode:{to}"))
                .or_else(|| name.strip_prefix(&format!("label:{from}:")).map(|base| format!("label:{to}:{base}")));
            if let Some(next) = next {
                t.put(&keys::sequence(&next), &bytes)?;
                t.del(&key)?;
            }
        }
    }
    derive::rebuild(t, None)
}
