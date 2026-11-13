//! The admin operations a migration runs over stored data (`docs/architecture/schema.md`, Changing a schema): deleting what a removed
//! declaration left (`purge`), and renaming, which a schema change never infers. Each runs in one unit.

use rmpv::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::schema::ValueType;

use super::changes::Event;
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

/// Refuses one of the engine's own fields (`RESERVED_FIELD`). No schema declares `shortCode`, `trashedAt` or the rest,
/// so none reads as what a removed declaration left or as a name to move data from; each is an entity's identity or
/// its state, kept by the writes that own it
fn refuse_engine_field(name: &str, via: &str) -> Result<()> {
    if !field::RESERVED.contains(&name) {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::ReservedField,
        format!(
            "{via}: \"{name}\" is the engine's own field, written with the entity it describes: it is neither purged nor renamed"
        ),
    ))
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
            refuse_engine_field(name, "purge")?;
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

/// Moves a field's values on every entity of a type to another name the type declares. Refused, with nothing moved,
/// when an entity holds a value under both names: the move would replace the one the new name holds, and which of the
/// two to keep is the migration's to say
pub fn rename_field(t: &mut Txn<'_, '_>, entity_type: &str, from: &str, to: &str) -> Result<()> {
    refuse_engine_field(from, "renameField")?;
    refuse_engine_field(to, "renameField")?;
    requires_declared(
        t.schema.types.get(entity_type).is_some_and(|d| d.fields.contains_key(to)),
        &format!("{entity_type}.{to}"),
    )?;
    for id in ids_of_type(t, entity_type)? {
        if let Some(value) = entities::get(t, &id, from)? {
            if entities::get(t, &id, to)?.is_some() {
                return Err(Error::new(
                    ErrorCode::SchemaChangeRefused,
                    format!(
                        "renameField: {id} holds both {entity_type}.{from} and {entity_type}.{to}, and the move would \
                         replace the second: delete the one not to keep from each such row, then rename"
                    ),
                ));
            }
            entities::delete_field(t, &id, from)?;
            entities::put_field(t, &id, to, &value)?;
        }
    }
    Ok(())
}

/// Moves every relation of a kind to another kind the schema declares, keeping their ids, ends, info and weights.
/// Refused, with nothing moved, when the new kind couldn't hold the edges: info the kind doesn't take, a weight a
/// weighted kind doesn't, an edge that would be a second of one identity, and an edge the kind's rules refuse a link
/// of (an entity joined to itself, the types it joins, an end with more far ends than the kind's cardinality gives it,
/// a cycle in a kind declared acyclic)
pub fn rename_relation(t: &mut Txn<'_, '_>, from: &str, to: &str) -> Result<()> {
    requires_declared(t.schema.relations.contains_key(to), &format!("The relation kind {to}"))?;
    if from == to {
        return Ok(());
    }
    let moving: Vec<(String, relations::Stored)> =
        all_relations(t)?.into_iter().filter(|(_, stored)| stored.kind == from).collect();
    // The schema outlives the unit's borrow of it, so the kind's rules are read while its edges are written
    let schema = t.schema;
    let def = schema.relations.get(to).expect("declared above");
    // What a rule of the new kind refuses, of one relation: the rule as a link of the kind is told it, and what to do.
    // A count of far ends and a cycle are broken by two relations or more, the others of the kind already or moved to
    // it before this one, so the link refused is one made beside those; every other rule is broken by the relation
    // alone. A write changes a relation while its kind is declared, and a load whatever its kind; a weight out of a
    // link's range came by a load, and a load gives the relation another whatever its kind. An error that is no
    // rule's, a read that failed, is answered as it is
    let change = match schema.relations.contains_key(from) {
        true => "Remove or change that relation",
        false => "Remove that relation, or bulkLoad it again under its id, changed",
    };
    let refused = |id: &str, stored: &relations::Stored, rule: Error| {
        let (as_a_link, to_do) = match rule.code {
            ErrorCode::Cardinality | ErrorCode::Cycle => (
                format!(
                    "Beside the relations of kind {to} and those of kind {from} the rename moves before it, a link of \
                     it would be refused"
                ),
                format!(
                    "{change}, or do the same to one it can't be of kind {to} beside, which is of that kind already \
                     or of kind {from} with it"
                ),
            ),
            ErrorCode::ValueInvalid => (
                "A link of it is refused".to_owned(),
                "Remove that relation, or bulkLoad it again under its id with a weight in that range".to_owned(),
            ),
            ErrorCode::FieldType | ErrorCode::RequiredField | ErrorCode::SelfLink | ErrorCode::Endpoint => {
                ("A link of it is refused".to_owned(), change.to_owned())
            }
            _ => return rule,
        };
        Error::new(
            rule.code,
            format!(
                "renameRelation({from:?} to {to:?}): the relation {id}, from {} to {}, can't be of kind {to}, and nothing \
                 was moved. {as_a_link}: \"{}\". {to_do}, then rename again",
                stored.source, stored.target, rule.message
            ),
        )
    };
    // Its info first: info that is no map reads as a null key, which the identity below would take for the edge's. And
    // its weight, where the new kind's are read: one outside what a link is given, which only a load brings
    for (id, stored) in &moving {
        relations::check_info(def, to, stored.info.as_ref()).map_err(|rule| refused(id, stored, rule))?;
        if def.weighted {
            relations::weight_of(def, to, stored.weight).map_err(|rule| refused(id, stored, rule))?;
        }
    }
    // Then refused before anything moves: an edge of the new kind with the same ends and key would be one identity
    // twice, as an update of an edge onto another's identity is refused. Two moving edges that would be one identity
    // are refused the same way, compared by the identity's own equality among those with the same ends
    let mut by_ends: std::collections::BTreeMap<(String, String), Vec<Vec<rmpv::Value>>> = Default::default();
    for (_, stored) in &moving {
        let key = relations::key_values(def, stored.info.as_ref());
        let clash = relations::with_identity(t, def, &stored.source, to, &stored.target, &key, None)?;
        let group = by_ends.entry((stored.source.clone(), stored.target.clone())).or_default();
        let twin = group.iter().any(|other| relations::same_key(other, &key));
        let identity =
            if def.key.is_empty() { String::new() } else { format!(" with the same {}", def.key.join(", ")) };
        if clash.is_some() {
            return Err(Error::new(
                ErrorCode::DuplicateEdge,
                format!(
                    "renameRelation({from:?} to {to:?}): an edge of kind {to} already joins {} to {}{identity}, and the \
                     rename would make a second: remove one of them first",
                    stored.source, stored.target
                ),
            ));
        }
        if twin {
            return Err(Error::new(
                ErrorCode::DuplicateEdge,
                format!(
                    "renameRelation({from:?} to {to:?}): two edges of kind {from} join {} to {}{identity} that kind {to} \
                     would hold as one, and the rename would make one identity of two: remove one of them first",
                    stored.source, stored.target
                ),
            ));
        }
        group.push(key);
    }
    // Each edge is held to the kind's rules as it moves, against the edges the kind holds by then, those moved before
    // it among them: a rename the kind can hold whole is refused at none, whatever order its edges move in
    for (id, stored) in moving {
        relations::check_rules(t, def, to, &stored.source, &stored.target, None)
            .map_err(|rule| refused(&id, &stored, rule))?;
        relations::rekind(t, &id, stored, to)?;
    }
    Ok(())
}

/// Renames a type: every entity's id from `<from>-…` to `<to>-…`, and with it the relations' ends, the declared `ref`
/// fields that named it, and the type's sequences. A string field holding an id that isn't a declared `ref` isn't
/// rewritten. Derived data is rebuilt from what this leaves. `sequence` is the commit's: a reader of what changed in
/// either type from before it is told to start again, the ids it read having gone and the ones in their place carrying
/// the stamps those had. Refused, with nothing moved, when a row the rename writes would break a rule of the schema
/// under its new name: an entity the new type's fields don't take or that shares a unique value with one of the
/// type's own, a relation whose kind doesn't join the new type, a `ref` field that doesn't name it
pub fn rename_type(t: &mut Txn<'_, '_>, from: &str, to: &str, sequence: u64) -> Result<()> {
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
    // The rows the rename writes, as a rule one breaks names it, by the ids they have once it has: the entities moved,
    // the relations with an end among them and the entities with a `ref` to one
    let mut written: std::collections::BTreeSet<String> = moved.iter().map(|(_, new)| new.clone()).collect();
    for (old, new) in &moved {
        if entities::record(t, new)?.is_some() {
            return Err(Error::new(ErrorCode::IdInUse, format!("renameType: {new} exists already")));
        }
        let record = t.get(&keys::entity(old))?.ok_or_else(|| {
            Error::new(ErrorCode::Corrupted, format!("renameType: {old} is listed and has no record"))
        })?;
        t.put(&keys::entity(new), &record)?;
        t.del(&keys::entity(old))?;
        for (name, bytes) in entities::fields_stored(t, old)? {
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
            // A relation is named by its row's key where a rule it breaks is reported
            written.insert(derive::key_text(&keys::relation(&id)));
            // Its other end may be of a type the rename leaves alone, whose readers are told as a write would tell
            // them: the edge they hold names an id that is gone
            t.event(Event::RelationChanged {
                relation: id,
                kind: next.kind,
                source: next.source,
                target: next.target,
                moved_from: None,
            });
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
    // doesn't hold yet; it holds the entities `to` had before, whose refs may name the renamed ones too
    let new_ids: Vec<String> = moved.iter().map(|(_, new)| new.clone()).collect();
    for (ty, name) in refs {
        let mut ids = ids_of_type(t, &ty)?;
        if ty == to {
            ids.extend(new_ids.iter().cloned());
        }
        for id in ids {
            let Some(value) = entities::get(t, &id, &name)? else { continue };
            let rewrite = |v: &Value| v.as_str().and_then(renamed).map(Value::from).unwrap_or_else(|| v.clone());
            let next = match &value {
                Value::Array(items) => Value::Array(items.iter().map(rewrite).collect()),
                v => rewrite(v),
            };
            if next != value {
                t.put(&keys::value(&id, &name), &values::stored(&next))?;
                written.insert(id.clone());
                // A field of another type's entity, written as any write of it is
                t.event(Event::Field { id, field: name.clone() });
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
                // `to` may number entities of its own already: its sequence goes on from the later of the two, so no
                // number either handed out is handed out again
                let held = t.get(&keys::sequence(&next))?.map(|b| values::decode(&b)).transpose()?;
                let moving = values::decode(&bytes)?;
                let later = match held.as_ref().and_then(Value::as_u64) {
                    Some(held) => held.max(moving.as_u64().unwrap_or(0)).into(),
                    None => moving,
                };
                t.put(&keys::sequence(&next), &values::encode(&later))?;
                t.del(&key)?;
            }
        }
    }
    // Each row written is held to the schema's rules as `check()` reads them, from primary data, before any derived
    // data is rebuilt: a row of another's that broke a rule before the rename is left to `check()`. A rule two rows
    // break between them names both, and is the rename's when either was written
    let broken = derive::rule_violations(t)?
        .into_iter()
        .find(|violation| violation.subject.split(", ").any(|subject| written.contains(subject)));
    if let Some(violation) = broken {
        let was = |new: &str| moved.iter().find(|(_, id)| id == new).map(|(old, _)| old.as_str());
        // An entity moved is named with the id it has, and a relation as one, where `check()` names its row's key
        let relation = derive::key_text(&keys::relation(""));
        let subject: Vec<String> = violation
            .subject
            .split(", ")
            .map(|id| match (was(id), id.strip_prefix(&relation)) {
                (Some(old), _) => format!("{id} (now {old})"),
                (None, Some(relation)) => format!("the relation {relation}"),
                (None, None) => id.to_owned(),
            })
            .collect();
        return Err(Error::new(
            ErrorCode::SchemaChangeRefused,
            format!(
                "renameType({from:?} to {to:?}): renamed, {} {}, which the schema refuses, and nothing was moved. \
                 Change that row, or what the schema declares of {to}, then rename again",
                subject.join(", "),
                violation.message
            ),
        ));
    }
    super::changed::replaced(t.txn, t.db, t.schema, Some(&[from, to]), sequence)?;
    derive::rebuild(t, None)
}
