//! Applying a batch's operations: each one's rules, then its writes (`docs/architecture/engine.md`, Writes). An operation refused
//! writes nothing; a batch is applied in a savepoint, so a refusal part way through keeps none of it.

use std::collections::BTreeMap;

use rmpv::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::schema::{AdditionalFields, FieldDef, RESERVED_TYPES, TypeDef};

use super::batch::{Op, OpResult};
use super::entities::{self, field};
use super::unit::Txn;
use super::{ids, numbering, relations, values};

pub fn apply(t: &mut Txn<'_, '_>, op: &Op) -> Result<OpResult> {
    match op {
        Op::Create { entity_type, id, fields, short_code, created_at, updated_at } => {
            create(t, entity_type, id.as_deref(), fields, short_code.as_deref(), *created_at, *updated_at)
        }
        Op::Upsert { entity_type, id, fields, short_code } => upsert(t, entity_type, id, fields, short_code.as_deref()),
        Op::Set { id, fields, touch } => {
            existing(t, id)?;
            let checked = check_fields(t, id, fields, "set")?;
            for (name, value) in &checked {
                entities::put_field(t, id, name, value)?;
            }
            if *touch && !checked.is_empty() {
                stamp(t, id)?;
            }
            Ok(OpResult::Done)
        }
        Op::Append { id, field, value, touch } => {
            existing(t, id)?;
            let mut list = list_of(t, id, field, "append")?;
            list.push(value.clone());
            write_field(t, id, field, Value::Array(list), *touch, "append")
        }
        Op::Merge { id, field, value, touch } => {
            existing(t, id)?;
            refuse_reserved(field, "merge")?;
            let current = entities::get(t, id, field)?;
            write_field(t, id, field, values::merged(current.as_ref(), value), *touch, "merge")
        }
        Op::Delete { id, field, touch } => {
            existing(t, id)?;
            refuse_reserved(field, "delete")?;
            let entity_type = ids::type_of(id);
            if declared(t, entity_type, field).is_some_and(|f| !f.optional) {
                return Err(Error::new(
                    ErrorCode::RequiredField,
                    format!("delete(\"{field}\"): every {entity_type} holds it; set(\"{field}\", value) replaces it"),
                ));
            }
            if entities::get(t, id, field)?.is_some() {
                entities::delete_field(t, id, field)?;
                if *touch {
                    stamp(t, id)?;
                }
            }
            Ok(OpResult::Done)
        }
        Op::RemoveAt { id, field, index, touch } => {
            existing(t, id)?;
            let mut list = list_of(t, id, field, "removeAt")?;
            if (*index as usize) < list.len() {
                list.remove(*index as usize);
                write_field(t, id, field, Value::Array(list), *touch, "removeAt")?;
            }
            Ok(OpResult::Done)
        }
        Op::RemoveValue { id, field, value, touch } => {
            existing(t, id)?;
            let mut list = list_of(t, id, field, "removeValue")?;
            if let Some(at) = list.iter().position(|v| values::matches_removal(v, value)) {
                list.remove(at);
                write_field(t, id, field, Value::Array(list), *touch, "removeValue")?;
            }
            Ok(OpResult::Done)
        }
        Op::Trash { id, at } => {
            existing(t, id)?;
            if !entities::record(t, id)?.is_some_and(|r| r.trashed) {
                let at = at.unwrap_or(t.now);
                entities::put_field(t, id, field::TRASHED_AT, &number(at))?;
            }
            Ok(OpResult::Done)
        }
        Op::Restore { id } => {
            existing(t, id)?;
            entities::delete_field(t, id, field::TRASHED_AT)?;
            Ok(OpResult::Done)
        }
        Op::Grant { id, role } => {
            existing(t, id)?;
            declared_role(t, role)?;
            grant(t, id, role)?;
            Ok(OpResult::Done)
        }
        Op::Revoke { id, role } => {
            existing(t, id)?;
            declared_role(t, role)?;
            revoke(t, id, role)?;
            Ok(OpResult::Done)
        }
        Op::GrantExclusive { id, role, among } => {
            existing(t, id)?;
            declared_role(t, role)?;
            for other in among.iter().filter(|o| *o != id) {
                if entities::record(t, other)?.is_some() {
                    revoke(t, other, role)?;
                }
            }
            grant(t, id, role)?;
            Ok(OpResult::Done)
        }
        Op::Link { source, kind, target, info, weight } => {
            existing(t, source)?;
            let (id, linked) = relations::link(t, source, kind, target, info.clone(), *weight)?;
            Ok(OpResult::Relation { id, linked })
        }
        Op::Unlink { source, kind, target } => {
            existing(t, source)?;
            relations::unlink(t, source, kind.as_deref(), target.as_deref())?;
            Ok(OpResult::Done)
        }
        Op::UpdateRelation { relation, source, target, info, weight } => {
            relations::update(t, relation, source.as_deref(), target.as_deref(), info.clone(), *weight)?;
            Ok(OpResult::Done)
        }
        Op::RemoveRelation { relation } => {
            if relations::get(t, relation)?.is_none() {
                return Err(Error::new(
                    ErrorCode::NoSuchRelation,
                    format!("removeRelation(\"{relation}\"): no relation has this id"),
                ));
            }
            relations::remove(t, relation)?;
            Ok(OpResult::Done)
        }
        Op::Destroy { id } => {
            existing(t, id)?;
            relations::remove_all_of(t, id)?;
            entities::destroy(t, id)?;
            Ok(OpResult::Done)
        }
        Op::TakeLabel { entity_type, base } => {
            type_def(t, entity_type, "takeLabel")?;
            Ok(OpResult::Label { label: numbering::next_label(t, entity_type, base)? })
        }
        Op::Embed { id, index, chunk, hash, model, vector } => {
            match super::vector::embed(t, id, index.as_deref(), *chunk, hash, model, vector)? {
                super::vector::Embedded::Stored => Ok(OpResult::Done),
                super::vector::Embedded::Stale => Ok(OpResult::Stale),
            }
        }
    }
}

fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 { Value::from(n as i64) } else { Value::from(n) }
}

/// Stamps the entity's `updatedAt` with the batch's time
fn stamp(t: &mut Txn<'_, '_>, id: &str) -> Result<()> {
    let now = number(t.now);
    entities::put_field(t, id, field::UPDATED_AT, &now)
}

/// The type's declaration, refusing a type the schema doesn't declare
fn type_def<'s>(t: &Txn<'s, '_>, entity_type: &str, via: &str) -> Result<&'s TypeDef> {
    if RESERVED_TYPES.contains(&entity_type) {
        return Err(Error::new(
            ErrorCode::ReservedType,
            format!(
                "{via}(\"{entity_type}\"): the engine's own type, which its ids are written with: no entity is created as one"
            ),
        ));
    }
    t.schema.types.get(entity_type).ok_or_else(|| {
        let hint = if entity_type.contains('-') { " It looks like an id: use write(id)." } else { "" };
        Error::new(ErrorCode::UnknownType, format!("{via}(\"{entity_type}\"): not a type the schema declares.{hint}"))
    })
}

fn declared<'s>(t: &Txn<'s, '_>, entity_type: &str, field: &str) -> Option<&'s FieldDef> {
    t.schema.types.get(entity_type)?.fields.get(field)
}

fn declared_role(t: &Txn<'_, '_>, role: &str) -> Result<()> {
    if t.schema.roles.contains_key(role) {
        Ok(())
    } else {
        Err(Error::new(ErrorCode::UnknownRole, format!("\"{role}\" isn't a role the schema declares")))
    }
}

/// Refuses an id no entity has, naming where a relation's id goes
fn existing(t: &Txn<'_, '_>, id: &str) -> Result<()> {
    if entities::record(t, id)?.is_some() {
        return Ok(());
    }
    let message = if relations::get(t, id)?.is_some() {
        format!(
            "write(\"{id}\"): that is a relation's id, and a relation is no entity. updateRelation(id, patch) changes it; removeRelation(id) removes it"
        )
    } else {
        format!(
            "write(\"{id}\"): no entity has this id. create(type) makes one; upsert(type, id) makes it when missing"
        )
    };
    Err(Error::new(ErrorCode::NoSuchEntity, message))
}

fn refuse_reserved(name: &str, via: &str) -> Result<()> {
    if !field::RESERVED.contains(&name) {
        return Ok(());
    }
    let advice = match name {
        field::CREATED_AT => "Pass it to create(type, { createdAt }).",
        field::ROLE => "Use grant() and revoke().",
        field::SHORT_CODE => {
            "create numbers it from the prefix the type declares; an import passes it to create(type, { shortCode })."
        }
        _ => "It is set when the entity is created.",
    };
    Err(Error::new(
        ErrorCode::ReservedField,
        format!("{via}('{name}'): \"{name}\" is written by the engine, not through {via}. {advice}"),
    ))
}

/// Checks a field and its value against the entity's type: declared (unless the type allows others), in the value
/// spec, and of the field's type
fn check_value(t: &Txn<'_, '_>, entity_type: &str, name: &str, value: &Value, via: &str) -> Result<()> {
    refuse_reserved(name, via)?;
    values::check_spec(value)?;
    let def = t.schema.types.get(entity_type);
    match def.and_then(|d| d.fields.get(name)) {
        Some(field_def) => values::check_field(entity_type, name, field_def, value),
        None if def.is_some_and(|d| d.additional_fields == AdditionalFields::Allow) => Ok(()),
        None => Err(Error::new(
            ErrorCode::UnknownField,
            format!("{via}('{name}'): {entity_type} declares no field \"{name}\", and takes none it doesn't declare"),
        )),
    }
}

/// Every field of a write checked before any is written
fn check_fields(
    t: &Txn<'_, '_>,
    id: &str,
    fields: &BTreeMap<String, Value>,
    via: &str,
) -> Result<Vec<(String, Value)>> {
    let entity_type = ids::type_of(id);
    fields
        .iter()
        .map(|(name, value)| check_value(t, entity_type, name, value, via).map(|()| (name.clone(), value.clone())))
        .collect()
}

/// Writes one field after checking it, and stamps `updatedAt` when `touch`
fn write_field(t: &mut Txn<'_, '_>, id: &str, name: &str, value: Value, touch: bool, via: &str) -> Result<OpResult> {
    check_value(t, ids::type_of(id), name, &value, via)?;
    entities::put_field(t, id, name, &value)?;
    if touch {
        stamp(t, id)?;
    }
    Ok(OpResult::Done)
}

/// The field's list, refusing a value that isn't one (`NOT_A_LIST`)
fn list_of(t: &Txn<'_, '_>, id: &str, name: &str, via: &str) -> Result<Vec<Value>> {
    refuse_reserved(name, via)?;
    match entities::get(t, id, name)? {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(Error::new(ErrorCode::NotAList, format!("{via}('{name}'): {id}'s value is not a list"))),
    }
}

fn roles_of(t: &Txn<'_, '_>, id: &str) -> Result<Vec<Value>> {
    Ok(match entities::get(t, id, field::ROLE)? {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    })
}

fn grant(t: &mut Txn<'_, '_>, id: &str, role: &str) -> Result<()> {
    let mut roles = roles_of(t, id)?;
    if !roles.iter().any(|r| r.as_str() == Some(role)) {
        roles.push(role.into());
        entities::put_field(t, id, field::ROLE, &Value::Array(roles))?;
    }
    Ok(())
}

fn revoke(t: &mut Txn<'_, '_>, id: &str, role: &str) -> Result<()> {
    let roles = roles_of(t, id)?;
    if !roles.iter().any(|r| r.as_str() == Some(role)) {
        return Ok(());
    }
    let rest: Vec<Value> = roles.into_iter().filter(|r| r.as_str() != Some(role)).collect();
    if rest.is_empty() {
        entities::delete_field(t, id, field::ROLE)
    } else {
        entities::put_field(t, id, field::ROLE, &Value::Array(rest))
    }
}

fn create(
    t: &mut Txn<'_, '_>,
    entity_type: &str,
    given: Option<&str>,
    fields: &BTreeMap<String, Value>,
    brought: Option<&str>,
    created_at: Option<f64>,
    updated_at: Option<f64>,
) -> Result<OpResult> {
    let def = type_def(t, entity_type, "create")?.clone();
    let quoted = format!("\"{entity_type}\"");
    for (name, value) in fields {
        check_value(t, entity_type, name, value, "create")?;
    }
    let prefix = def.short_code.as_ref().map(|c| c.prefix.clone());
    if let Some(code) = brought {
        let Some(prefix) = &prefix else {
            return Err(Error::new(
                ErrorCode::InvalidShortCode,
                format!(
                    "create({quoted}, {{ shortCode: \"{code}\" }}): a {entity_type} is not numbered, so it holds no short code"
                ),
            ));
        };
        if !numbering::is_short_code_of(code, prefix) {
            return Err(Error::new(
                ErrorCode::InvalidShortCode,
                format!("create({quoted}, {{ shortCode: \"{code}\" }}): a {entity_type}'s short code is {prefix}-<n>"),
            ));
        }
    }
    let missing: Vec<String> = def
        .fields
        .iter()
        .filter(|(name, f)| !f.optional && !fields.contains_key(*name))
        .map(|(name, _)| format!("\"{name}\""))
        .collect();
    if !missing.is_empty() {
        return Err(Error::new(
            ErrorCode::RequiredField,
            format!("create({quoted}): the row lacks {}, which every {entity_type} holds", missing.join(", ")),
        ));
    }
    let id = match given {
        Some(given) => {
            if ids::type_of(given) != entity_type {
                return Err(Error::new(
                    ErrorCode::TypeMismatch,
                    format!("create({quoted}, {{ id: \"{given}\" }}): the id names another type"),
                ));
            }
            if entities::record(t, given)?.is_some() {
                return Err(Error::new(
                    ErrorCode::IdInUse,
                    format!(
                        "create({quoted}, {{ id: \"{given}\" }}): an entity already has this id; write(id) changes it, upsert(type, id) does either"
                    ),
                ));
            }
            given.to_owned()
        }
        None => ids::new_entity_id(entity_type, t.now),
    };
    entities::insert(t, &id)?;
    let short_code = match (brought, &prefix) {
        (Some(code), _) => Some(code.to_owned()),
        (None, Some(prefix)) => Some(numbering::next_short_code(t, entity_type, prefix)?),
        (None, None) => None,
    };
    if let Some(code) = &short_code {
        // The short code's unique index refuses one another entity holds (`SHORT_CODE_IN_USE`)
        entities::put_field(t, &id, field::SHORT_CODE, &code.as_str().into())?;
    }
    let at = created_at.unwrap_or(t.now);
    entities::put_field(t, &id, field::CREATED_AT, &number(at))?;
    entities::put_field(t, &id, field::UPDATED_AT, &number(updated_at.unwrap_or(at)))?;
    for (name, value) in fields {
        entities::put_field(t, &id, name, value)?;
    }
    Ok(OpResult::Entity { id, short_code, created: true })
}

fn upsert(
    t: &mut Txn<'_, '_>,
    entity_type: &str,
    id: &str,
    fields: &BTreeMap<String, Value>,
    short_code: Option<&str>,
) -> Result<OpResult> {
    type_def(t, entity_type, "upsert")?;
    if entities::record(t, id)?.is_none() {
        return create(t, entity_type, Some(id), fields, short_code, None, None);
    }
    let quoted = format!("\"{entity_type}\"");
    if ids::type_of(id) != entity_type {
        return Err(Error::new(
            ErrorCode::TypeMismatch,
            format!("upsert({quoted}, {{ id: \"{id}\" }}): that entity is a {}", ids::type_of(id)),
        ));
    }
    let held = entities::get(t, id, field::SHORT_CODE)?.and_then(|v| v.as_str().map(str::to_owned));
    if let Some(code) = short_code
        && held.as_deref() != Some(code)
    {
        return Err(Error::new(
            ErrorCode::InvalidShortCode,
            format!(
                "upsert({quoted}, {{ id: \"{id}\", shortCode: \"{code}\" }}): the entity's short code is {held:?}, and nothing changes it"
            ),
        ));
    }
    let checked = check_fields(t, id, fields, "upsert")?;
    for (name, value) in &checked {
        entities::put_field(t, id, name, value)?;
    }
    if !checked.is_empty() {
        stamp(t, id)?;
    }
    Ok(OpResult::Entity { id: id.to_owned(), short_code: held, created: false })
}
