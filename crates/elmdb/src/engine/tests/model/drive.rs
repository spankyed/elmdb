//! Each step as the engine is asked it: the operation a step is, and how it runs, alone, in a unit or as an
//! administrative call

use super::*;

/// The first relation between the ends, or an id no relation has, so the engine refuses the operation as the model does
pub(super) fn relation_between(db: &Database, source: &str, kind: &str, target: &str) -> String {
    db.snapshot(|s| relations::between(s, source, kind, target))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| "Relation-none".to_owned())
}

/// The `cites` relation between the ends holding the key `n`, or an id no relation has
pub(super) fn keyed_between(r: &impl Read, source: &str, target: &str, n: u8) -> String {
    let key = rmpv::Value::Map(vec![("n".into(), n.into())]);
    relations::between(r, source, KEYED, target)
        .unwrap()
        .into_iter()
        .find(|relation| relations::get(r, relation).unwrap().is_some_and(|row| row.info.as_ref() == Some(&key)))
        .unwrap_or_else(|| "Relation-none".to_owned())
}

/// The engine operation of one write
pub(super) fn op_of(db: &Database, step: &Step) -> Json {
    match step {
        Step::Upsert { id, fields } => {
            json!({ "op": "upsert", "type": crate::engine::ids::type_of(id), "id": id, "fields": fields })
        }
        Step::Merge { id, key, value } => json!({ "op": "merge", "id": id, "field": key, "value": value }),
        Step::RemoveAt { id, key, index } => json!({ "op": "removeAt", "id": id, "field": key, "index": index }),
        Step::RemoveValue { id, key, value } => json!({ "op": "removeValue", "id": id, "field": key, "value": value }),
        Step::GrantExclusive { id, role } => json!({ "op": "grantExclusive", "id": id, "role": role, "among": IDS }),
        Step::RemoveRelation { id, kind, target } => {
            json!({ "op": "removeRelation", "relation": relation_between(db, id, kind, target) })
        }
        Step::MoveRelation { id, kind, target, to } => {
            json!({ "op": "updateRelation", "relation": relation_between(db, id, kind, target), "target": to })
        }
        Step::Rekey { id, target, from, to } => {
            let relation = db.snapshot(|s| Ok(keyed_between(s, id, target, *from))).unwrap();
            json!({ "op": "updateRelation", "relation": relation, "info": { "n": to } })
        }
        Step::TakeLabel { id } => {
            json!({ "op": "takeLabel", "type": crate::engine::ids::type_of(id), "base": "Untitled" })
        }
        other => op_of_plain(other),
    }
}

/// The engine operation of one write inside a unit, whose own writes a relation's lookup must see
pub(super) fn op_of_unit(unit: &mut crate::Unit, step: &Step) -> Json {
    let mut found = |source: &str, kind: &str, target: &str| {
        unit.read_with(|s| relations::between(s, source, kind, target))
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or_else(|| "Relation-none".to_owned())
    };
    match step {
        Step::RemoveRelation { id, kind, target } => {
            json!({ "op": "removeRelation", "relation": found(id, kind, target) })
        }
        Step::MoveRelation { id, kind, target, to } => {
            json!({ "op": "updateRelation", "relation": found(id, kind, target), "target": to })
        }
        Step::Rekey { id, target, from, to } => {
            let relation = unit.read_with(|s| Ok(keyed_between(s, id, target, *from))).unwrap();
            json!({ "op": "updateRelation", "relation": relation, "info": { "n": to } })
        }
        other => op_of_plain(other),
    }
}

pub(super) fn op_of_plain(step: &Step) -> Json {
    match step {
        Step::Upsert { id, fields } => {
            json!({ "op": "upsert", "type": crate::engine::ids::type_of(id), "id": id, "fields": fields })
        }
        Step::Merge { id, key, value } => json!({ "op": "merge", "id": id, "field": key, "value": value }),
        Step::RemoveAt { id, key, index } => json!({ "op": "removeAt", "id": id, "field": key, "index": index }),
        Step::RemoveValue { id, key, value } => json!({ "op": "removeValue", "id": id, "field": key, "value": value }),
        Step::GrantExclusive { id, role } => json!({ "op": "grantExclusive", "id": id, "role": role, "among": IDS }),
        Step::TakeLabel { id } => {
            json!({ "op": "takeLabel", "type": crate::engine::ids::type_of(id), "base": "Untitled" })
        }
        Step::Create { id, fields } => {
            json!({ "op": "create", "type": crate::engine::ids::type_of(id), "id": id, "fields": fields })
        }
        Step::Set { id, key, value } => json!({ "op": "set", "id": id, "fields": { *key: value } }),
        Step::Append { id, key, value } => json!({ "op": "append", "id": id, "field": key, "value": value }),
        Step::Delete { id, key } => json!({ "op": "delete", "id": id, "field": key }),
        Step::Link { id, kind, target } => json!({ "op": "link", "source": id, "kind": kind, "target": target }),
        Step::Unlink { id, kind, target } => json!({ "op": "unlink", "source": id, "kind": kind, "target": target }),
        Step::LinkKeyed { id, target, n } => {
            json!({ "op": "link", "source": id, "kind": KEYED, "target": target, "info": { "n": n } })
        }
        Step::Destroy { id } => json!({ "op": "destroy", "id": id }),
        Step::Trash { id } => json!({ "op": "trash", "id": id }),
        Step::Restore { id } => json!({ "op": "restore", "id": id }),
        Step::Grant { id, role } => json!({ "op": "grant", "id": id, "role": role }),
        Step::Revoke { id, role } => json!({ "op": "revoke", "id": id, "role": role }),
        Step::RemoveRelation { .. }
        | Step::MoveRelation { .. }
        | Step::Rekey { .. }
        | Step::Unit { .. }
        | Step::RefuseNext
        | Step::Reset
        | Step::RenameRelation { .. }
        | Step::RenameField { .. }
        | Step::Backup
        | Step::CloseGivesUp
        | Step::ReplaceWith
        | Step::Redeclare
        | Step::PurgeField
        | Step::PurgeKind
        | Step::RenameTypeAndBack
        | Step::Rebuild { .. } => {
            unreachable!("not a write on its own")
        }
    }
}

/// Applies a step's writes inside an open unit, as the model's `apply` does
pub(super) fn apply_in(unit: &mut crate::Unit, step: &Step) -> Result<(), crate::Error> {
    match step {
        Step::Unit { steps, throws, .. } => {
            let depth = unit.savepoint()?;
            let outcome = steps.iter().try_for_each(|s| apply_in(unit, s));
            if outcome.is_err() || *throws { unit.rollback_to(depth) } else { unit.release(depth) }
        }
        write => {
            let op = op_of_unit(unit, write);
            unit.write(&super::super::batch(vec![op])).map(|_| ())
        }
    }
}

/// Runs one step on the engine: the change set of the commit it made, or `None` when it made none
pub(super) fn run(db: &Database, step: &Step) -> Result<Option<ChangeSet>, crate::Error> {
    match step {
        Step::RefuseNext => {
            crate::failpoints::fail_once_at(crate::failpoints::points::COMMIT, db.path());
            Ok(None)
        }
        Step::Rebuild { index } => db.rebuild(*index).map(|()| None),
        Step::Reset => db.reset().map(|()| None),
        Step::RenameRelation { from, to } => db.rename_relation(from, to).map(|()| None),
        Step::RenameField { from, to } => db.rename_field("Task", from, to).map(|()| None),
        Step::CloseGivesUp => {
            // The store held past the close's wait, as an operation under way holds it
            let held = db.store()?;
            let closed = db.close_within(Some(std::time::Duration::from_millis(1)));
            drop(held);
            assert_eq!(closed.map_err(|refused| refused.code), Ok(false), "a close given up on for the store held");
            Ok(None)
        }
        Step::Backup => {
            // Into the database's own directory, so that the copy goes with it; the last backup is the one replaced with
            let target = db.path().join("model-backup");
            let _ = std::fs::remove_dir_all(&target);
            db.backup(&target).map(|()| None)
        }
        Step::ReplaceWith => db.replace_with(db.path().join("model-backup")).map(|()| None),
        Step::Redeclare => {
            let declared = db.schema().is_some_and(|schema| schema.relations.contains_key("tags"));
            db.set_schema(Schema::from_json(&harness_schema(!declared)).unwrap()).map(|_| None)
        }
        Step::PurgeField => db
            .purge(&crate::engine::admin::Purge::Field { entity_type: "Task".into(), field: "spare".into() })
            .map(|()| None),
        Step::PurgeKind => db.purge(&crate::engine::admin::Purge::RelationKind("tags".into())).map(|()| None),
        Step::RenameTypeAndBack => {
            // The two renames `Database::rename_type` makes one unit each, here in one, at the commit's sequence. A move
            // both renames skipped would leave what was there, so between them `Memo` must hold what `Note` held
            use crate::engine::admin::rename_type;
            let store = db.store()?;
            db.admin_unit(|t| {
                let sequence = store.next_sequence(t.txn)?;
                let before = by_number(t, "Note")?;
                rename_type(t, "Note", "Memo", sequence)?;
                assert_eq!(by_number(t, "Memo")?, before, "Memo holds what Note held, by the ids' numbers");
                assert_eq!(by_number(t, "Note")?, ByNumber::default(), "Note holds nothing once renamed");
                rename_type(t, "Memo", "Note", sequence)
            })
            .map(|()| None)
        }
        Step::Unit { steps, throws, durable } => {
            let mut unit = db.begin()?;
            let outcome = steps.iter().try_for_each(|s| apply_in(&mut unit, s));
            if let Err(e) = outcome {
                unit.abort();
                return Err(e);
            }
            if *throws {
                unit.abort();
                return Err(crate::Error::new(crate::ErrorCode::Io, "the unit threw"));
            }
            unit.commit(*durable).map(Some)
        }
        write => db.write(&super::super::batch(vec![op_of(db, write)]), false).map(|(_, changes)| Some(changes)),
    }
}

/// What a type's entities hold with each id of the type named by its number alone: each entity's row and stored
/// values, and the kind, ends and info of each relation with an end among them
#[derive(Debug, Default, PartialEq)]
pub(super) struct ByNumber {
    pub(super) entities: BTreeMap<String, (Vec<u8>, StoredValues)>,
    pub(super) relations: BTreeSet<(String, String, String, String)>,
}

/// An entity's stored values, each field's name with its bytes
pub(super) type StoredValues = Vec<(String, Vec<u8>)>;

pub(super) fn by_number(t: &impl Read, entity_type: &str) -> crate::Result<ByNumber> {
    let number = |id: &str| {
        (crate::engine::ids::type_of(id) == entity_type).then(|| id.strip_prefix(entity_type).unwrap_or(id).to_owned())
    };
    let mut out = ByNumber::default();
    for (key, record) in t.scan(&keys::all_entities())? {
        if let Some(Part::String(id)) = decode_key(&key)?.pop()
            && let Some(n) = number(&id)
        {
            out.entities.insert(n, (record, entities::fields_stored(t, &id)?));
        }
    }
    for (_, bytes) in t.scan(&keys::all_relations())? {
        let stored: relations::Stored = rmp_serde::from_slice(&bytes).expect("a relation row reads");
        let (source, target) = (number(&stored.source), number(&stored.target));
        if source.is_some() || target.is_some() {
            let end = |n: Option<String>, id: String| n.map_or(id, |n| format!("#{n}"));
            let info = format!("{:?}", stored.info);
            out.relations.insert((stored.kind, end(source, stored.source), end(target, stored.target), info));
        }
    }
    Ok(out)
}
